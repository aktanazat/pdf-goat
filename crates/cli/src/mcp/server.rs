//! The MCP surface: the `capabilities`, `run` and `render` tools and the capabilities
//! resources, each served by one `pdf-goat --agent` child.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, JsonObject, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ResourceTemplate, ServerCapabilities, Tool,
    ToolAnnotations, object,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_util::task::TaskTracker;

use crate::child::{self, Failure, Finished};
use crate::reply;

const INSTRUCTIONS: &str = "\
pdf-goat reads, searches, renders, edits, fills, signs, redacts, OCRs, converts, compares \
and optimizes PDFs. Use `capabilities` to find a command and its arguments, `run` to run \
it, `render` to look at a page.

Paths are absolute. A change never touches its input; it writes the file named by `-o`. \
Before reporting a change, read the output back through another command (text, search, \
form list, security verify, compare) and look at the changed region with render.

Coordinates are PDF points from the top-left, y down. Search rects, the --at and --rect \
of edit add-text, the --rect of edit add-image, annotate, form create, links add and \
security sign, and render's marks are measured on the unrotated crop box, /Rotate \
ignored; add-text and add-image return the bbox they used in that frame. Render's clip is \
measured on the page as displayed, rotation applied. The two frames agree only on a page \
without /Rotate (inspect reports rotation).";

const CAPABILITIES: &str = "\
Describe pdf-goat's commands as JSON. Without `selector`: every command family and \
top-level command. With `selector`, one family (edit, form, security, ...) or one \
top-level command (info, search, render, ...): the exact arguments of each command in it. \
Look a command up here before calling `run` with it.";

const RUN: &str = "\
Run one pdf-goat command and return its JSON result. `command` is the command path: \
`info`, `text`, `search`, `edit add-text`, `form fill`, `security sign`. `args` is the \
rest of the command line, one token per item, exactly as the CLI takes it, for example \
[\"/abs/in.pdf\", \"--text\", \"Approved\", \"--page\", \"1\", \"--at\", \"72,72\", \
\"-o\", \"/abs/out.pdf\"]. A failed command (nonzero exit or \"ok\": false) comes back \
as an error carrying pdf-goat's message. PNG and JPEG files in the result's outputs come \
back as images: the first 4, each at most 5 MiB. A result over 24,000 bytes comes back \
shortened, with the path of a file that holds all of it. A command that writes replaces \
an existing -o file; `office run` executes scripts; `setup` downloads models.";

const RENDER: &str = "\
Render one page to a PNG and return the image, to look at a page or check a change. \
`page` counts from 1 (default 1); `dpi` defaults to 96. A rectangle is \"x0,y0,x1,y1\" \
or [x0, y0, x1, y1] in points, from the top-left, y down. `clip` renders only that \
rectangle of the page as displayed (rotation applied). `marks` outlines rectangles to \
check positions, for example search hits or the bbox an add-text or add-image call \
returned. Marks use search's frame (the unrotated crop box) and take a search `rect` or \
a returned `bbox` as is, also on rotated pages.";

const CAPABILITIES_URI: &str = "pdf-goat://capabilities";
const CAPABILITIES_TEMPLATE: &str = "pdf-goat://capabilities/{selector}";

pub struct Server {
    pdf_goat: PathBuf,
    session: Session,
    children: TaskTracker,
}

/// The server's scratch directory: render outputs and full copies of large results.
pub struct Session {
    dir: PathBuf,
    sequence: AtomicU64,
}

impl Session {
    /// A path in the session directory no earlier call used: `<dir>/<kind>-<n><suffix>`.
    pub fn fresh(&self, kind: &str, suffix: &str) -> PathBuf {
        let n = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.dir.join(format!("{kind}-{n}{suffix}"))
    }
}

impl Server {
    pub fn new(pdf_goat: PathBuf, session: PathBuf) -> Self {
        Self {
            pdf_goat,
            session: Session {
                dir: session,
                sequence: AtomicU64::new(0),
            },
            children: TaskTracker::new(),
        }
    }

    /// Tracks every running pdf-goat child, so shutdown can wait for all of them.
    pub fn children(&self) -> TaskTracker {
        self.children.clone()
    }

    async fn pdf_goat(
        &self,
        args: &[OsString],
        context: &RequestContext<RoleServer>,
    ) -> Result<Finished, Failure> {
        child::run(&self.pdf_goat, args, &self.children, context.ct.cancelled()).await
    }

    fn render_args(&self, args: RenderArgs) -> Vec<OsString> {
        let RenderArgs {
            file,
            page,
            dpi,
            clip,
            marks,
        } = args;
        let mut argv: Vec<OsString> = vec![
            "render".into(),
            file.into(),
            "--pages".into(),
            page.unwrap_or(1).to_string().into(),
            "--dpi".into(),
            dpi.unwrap_or(96).to_string().into(),
        ];
        // The `=` form keeps a negative first coordinate from reading as a flag.
        argv.extend(clip.map(|clip| format!("--clip={}", clip.arg()).into()));
        argv.extend(
            marks
                .unwrap_or_default()
                .into_iter()
                .map(|mark| format!("--mark={}", mark.arg()).into()),
        );
        argv.extend(["-o".into(), self.session.fresh("render", "").into()]);
        argv
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilitiesArgs {
    selector: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    command: String,
    args: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderArgs {
    file: String,
    page: Option<u32>,
    dpi: Option<u32>,
    clip: Option<Rect>,
    marks: Option<Vec<Rect>>,
}

/// A rectangle as the CLI's `"x0,y0,x1,y1"` text or as the `[x0, y0, x1, y1]` array a
/// `search` hit's `rect` and an edit's `bbox` carry.
#[derive(Deserialize)]
#[serde(
    untagged,
    expecting = "a rectangle, \"x0,y0,x1,y1\" or [x0, y0, x1, y1]"
)]
enum Rect {
    Text(String),
    Points([f64; 4]),
}

impl Rect {
    /// The rectangle as the CLI's `x0,y0,x1,y1` argument.
    fn arg(self) -> String {
        match self {
            Rect::Text(text) => text,
            Rect::Points([x0, y0, x1, y1]) => format!("{x0},{y0},{x1},{y1}"),
        }
    }
}

fn capabilities_args(selector: Option<String>) -> Vec<OsString> {
    std::iter::once("capabilities".to_owned())
        .chain(selector)
        .map(OsString::from)
        .collect()
}

fn run_args(RunArgs { command, args }: RunArgs) -> Vec<OsString> {
    let command = command.split_whitespace().map(OsString::from);
    command
        .chain(args.unwrap_or_default().into_iter().map(OsString::from))
        .collect()
}

fn parse<T: DeserializeOwned>(arguments: JsonObject) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments))
        .map_err(|error| format!("invalid arguments: {error}"))
}

fn tools() -> Vec<Tool> {
    let rect = |description: &str| {
        json!({
            "anyOf": [
                { "type": "string" },
                { "type": "array", "items": { "type": "number" }, "minItems": 4, "maxItems": 4 },
            ],
            "description": format!("\"x0,y0,x1,y1\" or [x0, y0, x1, y1] in PDF points, {description}"),
        })
    };
    vec![
        Tool::new(
            "capabilities",
            CAPABILITIES,
            object(json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "one command family (edit, form, ...) or top-level command (info, render, ...)",
                    },
                },
                "additionalProperties": false,
            })),
        )
        .with_annotations(
            ToolAnnotations::new()
                .read_only(true)
                .idempotent(true)
                .open_world(false),
        ),
        Tool::new(
            "run",
            RUN,
            object(json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "the command path, for example `info` or `edit add-text`",
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "the rest of the command line, one token per item",
                    },
                },
                "required": ["command"],
                "additionalProperties": false,
            })),
        )
        .with_annotations(
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(false)
                .open_world(true),
        ),
        Tool::new(
            "render",
            RENDER,
            object(json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "absolute path of the PDF" },
                    "page": { "type": "integer", "minimum": 1, "default": 1 },
                    "dpi": { "type": "integer", "minimum": 1, "default": 96 },
                    "clip": rect("on the page as displayed"),
                    "marks": {
                        "type": "array",
                        "items": rect("in search's frame"),
                        "description": "rectangles to outline",
                    },
                },
                "required": ["file"],
                "additionalProperties": false,
            })),
        )
        .with_annotations(
            ToolAnnotations::new()
                .read_only(true)
                .idempotent(true)
                .open_world(false),
        ),
    ]
}

impl ServerHandler for Server {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("pdf-goat", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.unwrap_or_default();
        let args = match request.name.as_ref() {
            "capabilities" => {
                parse(arguments).map(|args: CapabilitiesArgs| capabilities_args(args.selector))
            }
            "run" => parse(arguments).map(run_args),
            "render" => parse(arguments).map(|args| self.render_args(args)),
            unknown => {
                return Err(ErrorData::invalid_params(
                    format!("no tool named {unknown}"),
                    None,
                ));
            }
        };
        let result = match args {
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
            Ok(args) => match self.pdf_goat(&args, &context).await {
                Ok(finished) => reply::tool_result(&finished, &self.session).await,
                Err(failure) => {
                    CallToolResult::error(vec![ContentBlock::text(failure.to_string())])
                }
            },
        };
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(CAPABILITIES_URI, "capabilities")
                .with_description("Every pdf-goat command family and top-level command.")
                .with_mime_type("application/json"),
        ]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new(CAPABILITIES_TEMPLATE, "capabilities-selector")
                .with_description(
                    "The arguments of one command family (edit, form, ...) or top-level \
                     command (info, render, ...).",
                )
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let selector = if uri == CAPABILITIES_URI {
            None
        } else {
            let selector = uri
                .strip_prefix(CAPABILITIES_URI)
                .and_then(|rest| rest.strip_prefix('/'))
                .filter(|selector| !selector.is_empty() && !selector.contains('/'))
                .ok_or_else(|| {
                    ErrorData::resource_not_found(format!("no resource at {uri}"), None)
                })?;
            Some(selector.to_owned())
        };
        let finished = self
            .pdf_goat(&capabilities_args(selector), &context)
            .await
            .map_err(|failure| ErrorData::internal_error(failure.to_string(), None))?;
        let (json, failed) = reply::outcome(&finished);
        if failed {
            return Err(ErrorData::resource_not_found(
                reply::failure_message(&finished, json.as_ref()),
                None,
            ));
        }
        let text = match json {
            Some(value) => value.to_string(),
            None => String::from_utf8_lossy(&finished.stdout).into_owned(),
        };
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, uri).with_mime_type("application/json"),
        ])
        .into())
    }
}
