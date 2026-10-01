//! `pdf-goat-mcp` driven over stdio the way an MCP client drives it. Expected results
//! come from running the `pdf-goat` CLI itself on the same arguments.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use goat_fixtures::{PdfBuilder, text_pages};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The longest any one step may take before the test fails instead of hanging.
const DEADLINE: Duration = Duration::from_secs(60);
/// How often a step that waits on another process looks again.
const POLL: Duration = Duration::from_millis(10);
const MCP: &str = env!("CARGO_BIN_EXE_pdf-goat-mcp");

/// A scratch directory holding the fixtures and a `home` for `PDF_GOAT_HOME`.
struct Scratch(TempDir);

impl Scratch {
    fn new() -> Scratch {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("scratch dir");
        fs::create_dir(dir.path().join("home")).expect("home dir");
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    /// `pdf-goat --agent ARGV...` run directly, as the reference for the server.
    fn cli(&self, argv: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pdf-goat"))
            .arg("--agent")
            .args(argv)
            .env("PDF_GOAT_HOME", self.home())
            .output()
            .expect("pdf-goat runs")
    }

    /// A FIFO nobody writes to: `pdf-goat info` on it blocks until it is stopped.
    fn fifo(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        let status = Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "mkfifo {}", path.display());
        path
    }

    /// A copy of the server, in directory `name`, whose `pdf-goat` is [`STAND_IN`].
    fn stand_in(&self, name: &str) -> StandIn {
        let dir = self.path(name);
        fs::create_dir(&dir).expect("dir");
        let server = dir.join("pdf-goat-mcp");
        fs::copy(MCP, &server).expect("copy the server");
        let pdf_goat = dir.join("pdf-goat");
        fs::write(&pdf_goat, STAND_IN).expect("stand-in");
        fs::set_permissions(&pdf_goat, fs::Permissions::from_mode(0o755)).expect("chmod");
        StandIn {
            server,
            log: dir.join("log"),
        }
    }
}

/// Stands in for `pdf-goat` to show how the server stops a child, which `pdf-goat`
/// cannot show because it never outlives SIGTERM. Run as `--agent ignore-term` it logs
/// `term PID` on SIGTERM and keeps running; as `--agent finish-on-term` it logs
/// `term PID`, takes a moment, logs `done PID` and exits. It logs `start PID` once its
/// trap is set, and ends by itself once the server that started it is gone.
const STAND_IN: &str = r#"#!/bin/sh
log="${0%/*}/log"
case "$2" in
ignore-term) trap 'echo "term $$" >> "$log"' TERM ;;
finish-on-term) trap 'echo "term $$" >> "$log"; sleep 0.2; echo "done $$" >> "$log"; exit 0' TERM ;;
*) exit 2 ;;
esac
echo "start $$" >> "$log"
while kill -0 "$PPID" 2>/dev/null; do sleep 0.05; done
"#;

/// A server copy whose `pdf-goat` is [`STAND_IN`], and the log the stand-in writes.
struct StandIn {
    server: PathBuf,
    log: PathBuf,
}

impl StandIn {
    fn read_log(&self) -> String {
        match fs::read_to_string(&self.log) {
            Ok(log) => log,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => panic!("read {}: {error}", self.log.display()),
        }
    }

    fn logged(&self, line: &str) -> bool {
        self.read_log().lines().any(|logged| logged == line)
    }

    /// Waits until the log holds `line`.
    fn wait_for(&self, line: &str) {
        let deadline = Instant::now() + DEADLINE;
        while !self.logged(line) {
            assert!(
                Instant::now() < deadline,
                "the stand-in never logged {line:?}: {:?}",
                self.read_log()
            );
            thread::sleep(POLL);
        }
    }

    /// The stand-in's pid, once it has started and set its trap.
    fn started(&self) -> u32 {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let log = self.read_log();
            if let Some(pid) = log.lines().find_map(|line| line.strip_prefix("start ")) {
                return pid.parse().expect("a pid");
            }
            assert!(Instant::now() < deadline, "the stand-in never started");
            thread::sleep(POLL);
        }
    }
}

/// One `pdf-goat-mcp` process after the initialize handshake.
struct Client {
    server: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    last_id: u64,
}

impl Client {
    fn start(home: &Path) -> Client {
        Client::start_binary(Path::new(MCP), home)
    }

    fn start_binary(binary: &Path, home: &Path) -> Client {
        let mut server = Command::new(binary)
            .env("PDF_GOAT_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("pdf-goat-mcp starts");
        let stdout = server.stdout.take().expect("stdout is piped");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = server.stdin.take();
        let mut client = Client {
            server,
            stdin,
            lines,
            last_id: 0,
        };
        let initialized = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "mcp-test", "version": "0" },
            }),
        );
        assert!(
            initialized.get("result").is_some(),
            "initialize: {initialized}"
        );
        client.notify("notifications/initialized", json!({}));
        client
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{message}").expect("the server reads its stdin");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Sends a request without waiting for its response; returns its id.
    fn start_request(&mut self, method: &str, params: Value) -> u64 {
        self.last_id += 1;
        let id = self.last_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    fn response(&self, id: u64) -> Value {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("no response to request {id}: {error}"));
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("the server wrote {line:?}: {error}"));
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.start_request(method, params);
        self.response(id)
    }

    fn call(&mut self, tool: &str, arguments: Value) -> ToolResult {
        let response = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        );
        match response.get("result") {
            Some(result) => ToolResult(result.clone()),
            None => panic!("tools/call {tool}: {response}"),
        }
    }

    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// The server's exit status, once it has exited.
    fn exit_status(&mut self) -> ExitStatus {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.server.try_wait().expect("try_wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "pdf-goat-mcp did not exit");
            thread::sleep(POLL);
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close_stdin();
        let deadline = Instant::now() + DEADLINE;
        while matches!(self.server.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        // A server that ignored its closed stdin must not outlive the test.
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// The `result` of a `tools/call`.
struct ToolResult(Value);

impl ToolResult {
    fn is_error(&self) -> bool {
        self.0.get("isError").and_then(Value::as_bool) == Some(true)
    }

    fn blocks(&self, kind: &str) -> Vec<&Value> {
        self.0["content"]
            .as_array()
            .expect("content is an array")
            .iter()
            .filter(|block| block["type"] == kind)
            .collect()
    }

    fn texts(&self) -> Vec<&str> {
        self.blocks("text")
            .into_iter()
            .map(|block| block["text"].as_str().expect("text block"))
            .collect()
    }

    /// The first text block, which carries pdf-goat's JSON.
    fn json(&self) -> Value {
        let text = self.texts().first().copied().expect("a text block");
        serde_json::from_str(text).unwrap_or_else(|error| panic!("{text:?}: {error}"))
    }

    /// Each image block as (MIME type, decoded bytes).
    fn images(&self) -> Vec<(String, Vec<u8>)> {
        self.blocks("image")
            .into_iter()
            .map(|block| {
                let data = block["data"].as_str().expect("image data");
                let mime = block["mimeType"].as_str().expect("image MIME type");
                (
                    mime.to_owned(),
                    STANDARD.decode(data).expect("base64 image"),
                )
            })
            .collect()
    }
}

/// Width and height from a PNG's IHDR chunk.
fn png_size(png: &[u8]) -> (u32, u32) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    let number = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().expect("4 bytes"));
    (number(16), number(20))
}

fn compact_len(value: &Value) -> usize {
    value.to_string().len()
}

fn path_arg(path: &Path) -> String {
    path.to_str().expect("UTF-8 path").to_owned()
}

/// Opens `fifo` for writing once a reader holds it, which proves the reader started.
fn open_when_read(fifo: &Path) -> File {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
        {
            Ok(file) => return file,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                assert!(
                    Instant::now() < deadline,
                    "nothing opened {}",
                    fifo.display()
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("open {}: {error}", fifo.display()),
        }
    }
}

/// Whether anything still holds the FIFO behind `writer` open for reading.
fn still_read(writer: &mut File) -> bool {
    match writer.write(b"x") {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::BrokenPipe => false,
        Err(error) if error.kind() == ErrorKind::WouldBlock => true,
        Err(error) => panic!("write to the FIFO: {error}"),
    }
}

/// Whether process `pid` exists; a zombie nobody has reaped yet counts.
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("kill runs")
        .success()
}

#[test]
fn run_returns_what_pdf_goat_prints_and_flags_its_failures() {
    let scratch = Scratch::new();
    let doc = path_arg(&scratch.path("three.pdf"));
    fs::write(&doc, text_pages(&["alpha", "beta", "gamma"])).expect("fixture");
    let missing = path_arg(&scratch.path("missing.pdf"));
    let stamped = path_arg(&scratch.path("stamped.pdf"));
    // (command, args, whether pdf-goat fails)
    let cases: [(&str, Vec<&str>, bool); 4] = [
        ("info", vec![&doc], false),
        ("info", vec![&missing], true),
        ("info", vec!["--help"], false),
        (
            "edit add-text",
            vec![&doc, "--text", "Approved", "--at", "72,100", "-o", &stamped],
            false,
        ),
    ];
    let mut client = Client::start(&scratch.home());
    for (command, args, fails) in cases {
        let argv: Vec<&str> = command.split(' ').chain(args.iter().copied()).collect();
        let expected = scratch.cli(&argv);
        assert_eq!(!expected.status.success(), fails, "pdf-goat {argv:?}");
        let stdout = String::from_utf8(expected.stdout).expect("UTF-8 stdout");

        let result = client.call("run", json!({ "command": command, "args": args }));

        assert_eq!(result.is_error(), fails, "{argv:?}: {}", result.0);
        let text = result.texts().first().copied().expect("a text block");
        match serde_json::from_str::<Value>(&stdout) {
            Ok(json) => assert_eq!(result.json(), json, "{argv:?}"),
            Err(_) => assert_eq!(text, stdout, "{argv:?}"),
        }
    }
}

#[test]
fn a_failure_without_json_comes_back_with_pdf_goats_stderr() {
    let scratch = Scratch::new();
    let doc = path_arg(&scratch.path("one.pdf"));
    fs::write(&doc, text_pages(&["one"])).expect("fixture");
    // With a file for its home, pdf-goat cannot record the run and prints no JSON.
    let home = scratch.path("home-is-a-file");
    fs::write(&home, b"").expect("fixture");
    let expected = Command::new(env!("CARGO_BIN_EXE_pdf-goat"))
        .args(["--agent", "info", &doc])
        .env("PDF_GOAT_HOME", &home)
        .output()
        .expect("pdf-goat runs");
    assert!(!expected.status.success(), "fixture: pdf-goat fails");
    assert!(
        expected.stdout.is_empty(),
        "fixture: pdf-goat prints no JSON"
    );
    let stderr = String::from_utf8(expected.stderr).expect("UTF-8 stderr");
    let mut client = Client::start(&home);

    let result = client.call("run", json!({ "command": "info", "args": [doc] }));

    assert!(result.is_error(), "{}", result.0);
    let texts = result.texts();
    assert!(
        texts.iter().any(|text| text.ends_with(stderr.trim_end())),
        "{}",
        result.0
    );
}

#[test]
fn render_shows_the_requested_page_region_and_marks() {
    let scratch = Scratch::new();
    let doc = scratch.path("sizes.pdf");
    let mut builder = PdfBuilder::new();
    builder.page(216.0, 108.0).text(20.0, 40.0, "first");
    builder.page(144.0, 288.0).text(20.0, 40.0, "second");
    builder.save(&doc);
    let doc = path_arg(&doc);
    // (arguments beside `file`, expected PNG width and height, expected `marks`)
    let cases = [
        (json!({}), (288, 144), json!([])),
        (json!({ "page": 2, "dpi": 144 }), (288, 576), json!([])),
        (json!({ "clip": "18,9,90,45" }), (96, 48), json!([])),
        // Only the part on the page is drawn.
        (json!({ "clip": "-36,0,36,36" }), (48, 48), json!([])),
        (
            json!({ "marks": ["-4,10,50,50", "60,10,100,50"] }),
            (288, 144),
            json!([[-4.0, 10.0, 50.0, 50.0], [60.0, 10.0, 100.0, 50.0]]),
        ),
        // A rectangle may also be the array that search and edit bboxes carry.
        (json!({ "clip": [18, 9, 90, 45] }), (96, 48), json!([])),
        (
            json!({ "marks": [[-4, 10, 50, 50], "60,10,100,50"] }),
            (288, 144),
            json!([[-4.0, 10.0, 50.0, 50.0], [60.0, 10.0, 100.0, 50.0]]),
        ),
    ];
    let mut client = Client::start(&scratch.home());
    for (mut arguments, size, marks) in cases {
        arguments["file"] = json!(doc);

        let result = client.call("render", arguments.clone());

        assert!(!result.is_error(), "{arguments}: {}", result.0);
        let images = result.images();
        assert_eq!(images.len(), 1, "{arguments}");
        assert_eq!(images[0].0, "image/png", "{arguments}");
        assert_eq!(png_size(&images[0].1), size, "{arguments}");
        assert_eq!(result.json()["marks"], marks, "{arguments}");
    }
}

#[test]
fn a_misspelt_argument_is_refused_rather_than_ignored() {
    let scratch = Scratch::new();
    let doc = path_arg(&scratch.path("one.pdf"));
    fs::write(&doc, text_pages(&["one"])).expect("fixture");
    // Each call succeeds once its misspelt argument is dropped.
    let cases = [
        (
            "capabilities",
            json!({ "selector": "render", "family": "edit" }),
        ),
        (
            "run",
            json!({ "command": "info", "args": [doc], "arg": "--help" }),
        ),
        ("render", json!({ "file": doc, "pages": "2" })),
    ];
    let mut client = Client::start(&scratch.home());
    for (tool, arguments) in cases {
        let result = client.call(tool, arguments.clone());

        assert!(result.is_error(), "{tool} {arguments}: {}", result.0);
    }
}

#[test]
fn a_result_over_24000_bytes_is_summarized_and_saved_whole() {
    let scratch = Scratch::new();
    let doc = scratch.path("titled.pdf");
    let doc_arg = path_arg(&doc);
    let write_titled = |title_length: usize| {
        let mut builder = PdfBuilder::new();
        builder.page(200.0, 100.0);
        builder.info("Title", &"x".repeat(title_length));
        builder.save(&doc);
    };
    let reference = || {
        let output = scratch.cli(&["info", &doc_arg]);
        assert!(output.status.success(), "pdf-goat info");
        let json: Value = serde_json::from_slice(&output.stdout).expect("JSON");
        (json, output.stdout)
    };
    // Each title character adds one byte to the compact JSON.
    write_titled(20_000);
    let title_at_limit = 20_000 + 24_000 - compact_len(&reference().0);
    let mut client = Client::start(&scratch.home());

    write_titled(title_at_limit);
    let (whole, _) = reference();
    assert_eq!(compact_len(&whole), 24_000, "fixture sizing");
    let inline = client.call("run", json!({ "command": "info", "args": [doc_arg] }));
    assert!(!inline.is_error(), "{}", inline.0);
    assert_eq!(
        inline.texts().len(),
        1,
        "a 24000-byte result comes back whole"
    );
    assert_eq!(inline.json(), whole);

    write_titled(title_at_limit + 1);
    let (whole, stdout) = reference();
    assert_eq!(compact_len(&whole), 24_001, "fixture sizing");
    let summarized = client.call("run", json!({ "command": "info", "args": [doc_arg] }));
    assert!(!summarized.is_error(), "{}", summarized.0);
    let summary = summarized.json();
    let whole = whole.as_object().expect("an object");
    let summary = summary.as_object().expect("an object");
    assert!(
        summary.keys().eq(whole.keys()),
        "keys and their order are kept"
    );
    let mut shortened = 0;
    for (key, value) in whole {
        if compact_len(value) <= 1_000 {
            assert_eq!(summary[key], *value, "{key} is short and kept");
        } else {
            shortened += 1;
            assert!(summary[key].is_string(), "{key} becomes a marker");
            assert_ne!(summary[key], *value, "{key} becomes a marker");
        }
    }
    assert_eq!(shortened, 1, "only the title-bearing metadata is long");
    let note = summarized.texts()[1];
    let saved = note
        .split_whitespace()
        .last()
        .expect("the note ends in a path");
    assert_eq!(fs::read(saved).expect("the saved result"), stdout);
}

#[test]
fn image_outputs_come_back_inline_up_to_four_of_at_most_5_mib() {
    let scratch = Scratch::new();
    let noise_side = 1600;
    let mut state: u32 = 0x2545_f491;
    let noise: Vec<u8> = (0..noise_side * noise_side * 3)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state.to_be_bytes()[0]
        })
        .collect();
    let mut builder = PdfBuilder::new();
    let side = f64::from(noise_side);
    builder
        .page(side, side)
        .image_rgb([0.0, 0.0, side, side], noise_side, noise_side, &noise);
    builder.save(scratch.path("noise.pdf"));
    for pages in [1, 4, 5] {
        let texts = vec!["page"; pages];
        fs::write(
            scratch.path(&format!("pages-{pages}.pdf")),
            text_pages(&texts),
        )
        .expect("fixture");
    }
    // (fixture, dpi, rendered images, images inline)
    let cases = [
        ("pages-1.pdf", "18", 1, 1),
        ("pages-4.pdf", "18", 4, 4),
        ("pages-5.pdf", "18", 5, 4),
        ("noise.pdf", "72", 1, 0),
    ];
    let mut client = Client::start(&scratch.home());
    for (fixture, dpi, rendered, inline) in cases {
        let outdir = path_arg(&scratch.path(&format!("{fixture}-pages")));
        let args = [
            path_arg(&scratch.path(fixture)),
            "--dpi".into(),
            dpi.into(),
            "-o".into(),
            outdir,
        ];

        let result = client.call("run", json!({ "command": "render", "args": args }));

        assert!(!result.is_error(), "{fixture}: {}", result.0);
        let outputs: Vec<String> = result.json()["outputs"]
            .as_array()
            .expect("outputs")
            .iter()
            .map(|path| path.as_str().expect("a path").to_owned())
            .collect();
        assert_eq!(outputs.len(), rendered, "{fixture}");
        let small: Vec<Vec<u8>> = outputs
            .iter()
            .filter(|path| fs::metadata(path).expect("an output").len() <= 5 * 1024 * 1024)
            .map(|path| fs::read(path).expect("an output"))
            .collect();
        let shown: Vec<Vec<u8>> = result
            .images()
            .into_iter()
            .map(|(_, bytes)| bytes)
            .collect();
        assert_eq!(shown.len(), inline, "{fixture}");
        assert_eq!(
            shown,
            small[..inline],
            "{fixture}: the first small images, in order"
        );
        let noted = result.texts().len() == 2;
        assert_eq!(
            noted,
            inline < rendered,
            "{fixture}: a note says images were left out"
        );
    }
}

#[test]
fn cancelling_a_call_stops_its_pdf_goat_and_the_server_keeps_serving() {
    let scratch = Scratch::new();
    let fifo = scratch.fifo("stuck.pdf");
    let mut client = Client::start(&scratch.home());
    let id = client.start_request(
        "tools/call",
        json!({ "name": "run", "arguments": { "command": "info", "args": [path_arg(&fifo)] } }),
    );
    let mut writer = open_when_read(&fifo);

    client.notify(
        "notifications/cancelled",
        json!({ "requestId": id, "reason": "test" }),
    );

    let deadline = Instant::now() + DEADLINE;
    while still_read(&mut writer) {
        assert!(
            Instant::now() < deadline,
            "pdf-goat still reads after the cancel"
        );
        thread::sleep(POLL);
    }
    let result = client.call("capabilities", json!({}));
    assert!(!result.is_error(), "{}", result.0);
}

#[test]
fn a_cancelled_call_whose_child_ignores_sigterm_is_killed() {
    let scratch = Scratch::new();
    let stand_in = scratch.stand_in("ignores-term");
    let mut client = Client::start_binary(&stand_in.server, &scratch.home());
    let id = client.start_request(
        "tools/call",
        json!({ "name": "run", "arguments": { "command": "ignore-term" } }),
    );
    let pid = stand_in.started();

    client.notify(
        "notifications/cancelled",
        json!({ "requestId": id, "reason": "test" }),
    );

    stand_in.wait_for(&format!("term {pid}"));
    let deadline = Instant::now() + DEADLINE;
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the child that ignored SIGTERM still runs"
        );
        thread::sleep(POLL);
    }
    let pong = client.request("ping", json!({}));
    assert!(pong.get("result").is_some(), "{pong}");
}

#[test]
fn the_server_exits_only_after_stopping_running_calls() {
    #[derive(Debug, Clone, Copy)]
    enum Stop {
        CloseStdin,
        Sigterm,
    }
    let scratch = Scratch::new();
    for stop in [Stop::CloseStdin, Stop::Sigterm] {
        let stand_in = scratch.stand_in(&format!("{stop:?}"));
        let mut client = Client::start_binary(&stand_in.server, &scratch.home());
        client.start_request(
            "tools/call",
            json!({ "name": "run", "arguments": { "command": "finish-on-term" } }),
        );
        let pid = stand_in.started();

        match stop {
            Stop::CloseStdin => client.close_stdin(),
            Stop::Sigterm => {
                let server = client.server.id().to_string();
                let status = Command::new("kill")
                    .args(["-TERM", &server])
                    .status()
                    .expect("kill runs");
                assert!(status.success(), "kill -TERM {server}");
            }
        }

        let status = client.exit_status();
        assert!(status.success(), "{stop:?}: {status}");
        assert!(
            stand_in.logged(&format!("done {pid}")),
            "{stop:?}: the child did not finish on SIGTERM before the server exited: {:?}",
            stand_in.read_log()
        );
        assert!(!alive(pid), "{stop:?}: the child outlived the server");
    }
}

#[test]
fn without_pdf_goat_beside_it_the_server_exits_naming_the_missing_binary() {
    let scratch = Scratch::new();
    let alone = scratch.path("alone");
    fs::create_dir(&alone).expect("dir");
    let copy = alone.join("pdf-goat-mcp");
    fs::copy(MCP, &copy).expect("copy the server");

    let output = Command::new(&copy)
        .stdin(Stdio::null())
        .output()
        .expect("the copy runs");

    assert!(!output.status.success());
    let missing = alone
        .canonicalize()
        .expect("canonical dir")
        .join("pdf-goat");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&missing.display().to_string()), "{stderr}");
}

#[test]
fn a_symlinked_server_runs_the_pdf_goat_beside_its_target() {
    let scratch = Scratch::new();
    let linked = scratch.path("linked");
    fs::create_dir(&linked).expect("dir");
    let link = linked.join("pdf-goat-mcp");
    std::os::unix::fs::symlink(MCP, &link).expect("symlink");

    let mut client = Client::start_binary(&link, &scratch.home());

    let result = client.call("capabilities", json!({}));
    assert!(!result.is_error(), "{}", result.0);
}

#[test]
fn capabilities_resources_read_what_pdf_goat_lists() {
    let scratch = Scratch::new();
    let mut client = Client::start(&scratch.home());
    // (URI, the pdf-goat arguments it reads, or `None` when no such resource exists)
    let cases = [
        ("pdf-goat://capabilities", Some(vec!["capabilities"])),
        (
            "pdf-goat://capabilities/edit",
            Some(vec!["capabilities", "edit"]),
        ),
        (
            "pdf-goat://capabilities/bogus",
            Some(vec!["capabilities", "bogus"]),
        ),
        ("pdf-goat://elsewhere", None),
    ];
    for (uri, argv) in cases {
        let response = client.request("resources/read", json!({ "uri": uri }));
        let expected = argv.map(|argv| {
            let output = scratch.cli(&argv);
            let json: Value = serde_json::from_slice(&output.stdout).expect("JSON");
            (output.status.success(), json)
        });
        match expected {
            Some((true, json)) => {
                let contents = &response["result"]["contents"][0];
                assert_eq!(contents["uri"], uri, "{response}");
                assert_eq!(contents["mimeType"], "application/json", "{response}");
                let text = contents["text"].as_str().expect("text contents");
                let read: Value = serde_json::from_str(text).expect("JSON contents");
                assert_eq!(read, json, "{uri}");
            }
            Some((false, json)) => {
                assert_eq!(
                    response["error"]["message"], json["error"],
                    "{uri}: {response}"
                );
            }
            None => assert!(response.get("error").is_some(), "{uri}: {response}"),
        }
    }
}
