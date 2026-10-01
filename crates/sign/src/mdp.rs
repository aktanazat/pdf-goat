//! Modification detection and prevention (ISO 32000-1 12.8.2.2–12.8.2.4) as pyhanko reads
//! and writes it: the DocMDP permission of a certification signature, the field locks
//! (FieldMDP) a signature carries, and the `/Reference` entries `security sign` writes for
//! them.

use pdf_core::{Dict, Document, ObjRef, Object};

/// `MDPPerm`: the changes a DocMDP permission level (`/P`) allows after signing, strictest
/// first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    /// 1: no changes.
    NoChanges,
    /// 2: filling in forms and signing.
    FillForms,
    /// 3: also annotations.
    Annotate,
}

impl Permission {
    pub fn from_p(value: i64) -> Option<Permission> {
        match value {
            1 => Some(Permission::NoChanges),
            2 => Some(Permission::FillForms),
            3 => Some(Permission::Annotate),
            _ => None,
        }
    }

    pub fn p(self) -> i64 {
        match self {
            Permission::NoChanges => 1,
            Permission::FillForms => 2,
            Permission::Annotate => 3,
        }
    }
}

/// `FieldMDPSpec`: the form fields a lock covers.
pub enum FieldLock {
    All,
    Include(Vec<String>),
    Exclude(Vec<String>),
}

impl FieldLock {
    /// Reads a field's `/Lock` or a FieldMDP reference's `/TransformParams`.
    pub fn read(doc: &Document, dict: &Dict) -> Result<FieldLock, String> {
        let names = || -> Result<Vec<String>, String> {
            let fields = dict
                .get(b"Fields")
                .ok_or("/Fields is required when /Action is not /All")?;
            doc.resolve_array(fields)
                .map_err(|e| e.to_string())?
                .ok_or("the field lock's /Fields is not an array")?
                .iter()
                .map(|name| {
                    doc.resolve(name)
                        .map_err(|e| e.to_string())?
                        .as_string()
                        .map(|s| s.to_text())
                        .ok_or_else(|| "the field lock's /Fields holds a non-string".to_owned())
                })
                .collect()
        };
        match dict.get_name(b"Action") {
            Some(b"All") => Ok(FieldLock::All),
            Some(b"Include") => Ok(FieldLock::Include(names()?)),
            Some(b"Exclude") => Ok(FieldLock::Exclude(names()?)),
            Some(other) => Err(format!(
                "/{} is not a field lock action",
                String::from_utf8_lossy(other)
            )),
            None => Err("/Action is required.".to_owned()),
        }
    }

    /// `FieldMDPSpec.is_locked`: a listed name also covers the fields beneath it.
    pub fn locks(&self, field: &str) -> bool {
        let (listed, include) = match self {
            FieldLock::All => return true,
            FieldLock::Include(listed) => (listed, true),
            FieldLock::Exclude(listed) => (listed, false),
        };
        let covered = listed.iter().any(|scope| {
            field
                .strip_prefix(scope.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        });
        covered == include
    }
}

/// What a signature asks of the revisions after it: `EmbeddedPdfSignature.docmdp_level`
/// and `.fieldmdp`.
pub struct Policy {
    /// From the signature's DocMDP reference, else its field's `/Lock /P`.
    pub doc_mdp: Option<Permission>,
    /// From the signature's FieldMDP reference.
    pub lock: Option<FieldLock>,
}

impl Policy {
    pub fn read(doc: &Document, sig: &Dict, field: &Dict) -> Result<Policy, String> {
        let doc_mdp = match doc_mdp(doc, sig)? {
            Some(permission) => Some(permission),
            None => match lock_dict(doc, field)? {
                Some(lock) => lock_permission(doc, &lock)?,
                None => None,
            },
        };
        let lock = transform_params(doc, sig, b"FieldMDP")?
            .map(|params| FieldLock::read(doc, &params))
            .transpose()?;
        Ok(Policy { doc_mdp, lock })
    }
}

/// The document's certification (`read_certification_data`): the signature `/Perms
/// /DocMDP` names and the permission its DocMDP reference grants.
pub struct Certification {
    pub signature: Option<ObjRef>,
    pub permission: Option<Permission>,
}

pub fn certification(doc: &Document) -> Result<Option<Certification>, String> {
    let catalog = doc.catalog().map_err(|e| e.to_string())?;
    let Some(perms) = catalog
        .get(b"Perms")
        .map(|perms| doc.resolve_dict(perms))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
    else {
        return Ok(None);
    };
    let Some(value) = perms.get(b"DocMDP") else {
        return Ok(None);
    };
    let sig = doc
        .resolve_dict(value)
        .map_err(|e| e.to_string())?
        .ok_or("/Perms /DocMDP is not a signature dictionary")?;
    Ok(Some(Certification {
        signature: value.as_reference(),
        permission: doc_mdp(doc, &sig)?,
    }))
}

/// What a new signature imposes on later revisions (`SigMDPSetup`): the DocMDP permission
/// when it certifies or its field's lock sets one, and the field's lock.
pub struct Setup {
    pub certify: bool,
    pub permission: Option<Permission>,
    lock: Option<Dict>,
}

impl Setup {
    /// `--certify`'s permission and the lock of the existing field signed, if any. A lock
    /// that sets a stricter permission than `certify` wins, as in pyhanko.
    pub fn new(
        doc: &Document,
        certify: Option<Permission>,
        field: Option<&Dict>,
    ) -> Result<Setup, String> {
        let lock = match field {
            Some(field) => lock_dict(doc, field)?,
            None => None,
        };
        let mut permission = certify;
        if let Some(lock) = &lock {
            FieldLock::read(doc, lock)?;
            if let Some(locked) = lock_permission(doc, lock)? {
                permission = Some(permission.map_or(locked, |asked| asked.min(locked)));
            }
        }
        Ok(Setup {
            certify: certify.is_some(),
            permission,
            lock,
        })
    }

    /// The signature dictionary's `/Reference` entries: a DocMDP reference when it
    /// certifies, and a FieldMDP reference repeating the field's lock, which also carries
    /// the permission as Acrobat writes it.
    pub fn references(&self, catalog: ObjRef) -> Vec<Object> {
        let mut references = Vec::new();
        if let (true, Some(permission)) = (self.certify, self.permission) {
            let mut params = Dict::new();
            params.insert("Type", Object::name("TransformParams"));
            params.insert("V", Object::name("1.2"));
            params.insert("P", permission.p());
            references.push(Object::Dict(signature_reference("DocMDP", params)));
        }
        if let Some(lock) = &self.lock {
            let mut params = Dict::new();
            params.insert("Type", Object::name("TransformParams"));
            if let Some(action) = lock.get(b"Action") {
                params.insert("Action", action.clone());
            }
            if lock.get_name(b"Action") != Some(b"All")
                && let Some(fields) = lock.get(b"Fields")
            {
                params.insert("Fields", fields.clone());
            }
            params.insert("V", Object::name("1.2"));
            if let Some(permission) = self.permission {
                params.insert("P", permission.p());
            }
            let mut reference = signature_reference("FieldMDP", params);
            reference.insert("Data", catalog);
            references.push(Object::Dict(reference));
        }
        references
    }
}

fn signature_reference(method: &str, params: Dict) -> Dict {
    let mut reference = Dict::new();
    reference.insert("Type", Object::name("SigRef"));
    reference.insert("TransformMethod", Object::name(method));
    reference.insert("TransformParams", params);
    reference
}

/// The permission a signature's DocMDP reference grants (`_extract_docmdp_for_sig`).
fn doc_mdp(doc: &Document, sig: &Dict) -> Result<Option<Permission>, String> {
    let Some(params) = transform_params(doc, sig, b"DocMDP")? else {
        return Ok(None);
    };
    match params.get(b"P") {
        // ISO 32000-1 table 254: /P defaults to 2.
        None => Ok(Some(Permission::FillForms)),
        Some(value) => permission(doc, value).map(Some),
    }
}

/// The permission a field lock sets with `/P`, a PDF 2.0 addition.
fn lock_permission(doc: &Document, lock: &Dict) -> Result<Option<Permission>, String> {
    lock.get(b"P")
        .map(|value| permission(doc, value))
        .transpose()
}

fn permission(doc: &Document, value: &Object) -> Result<Permission, String> {
    doc.resolve(value)
        .map_err(|e| e.to_string())?
        .as_i64()
        .and_then(Permission::from_p)
        .ok_or_else(|| "Failed to read document permissions".to_owned())
}

/// The `/TransformParams` of the signature's reference with `/TransformMethod` `method`.
fn transform_params(doc: &Document, sig: &Dict, method: &[u8]) -> Result<Option<Dict>, String> {
    let Some(references) = sig.get(b"Reference") else {
        return Ok(None);
    };
    let references = doc
        .resolve_array(references)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    for reference in &references {
        let Some(reference) = doc.resolve_dict(reference).map_err(|e| e.to_string())? else {
            continue;
        };
        if reference.get_name(b"TransformMethod") == Some(method) {
            return reference
                .get(b"TransformParams")
                .map(|params| doc.resolve_dict(params))
                .transpose()
                .map_err(|e| e.to_string())?
                .flatten()
                .map(Some)
                .ok_or_else(|| "a signature reference has no /TransformParams".to_owned());
        }
    }
    Ok(None)
}

fn lock_dict(doc: &Document, field: &Dict) -> Result<Option<Dict>, String> {
    Ok(field
        .get(b"Lock")
        .map(|lock| doc.resolve_dict(lock))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten())
}
