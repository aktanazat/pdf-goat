//! RFC 5280 §6.1 path processing for what the certificate authorities of a chain impose
//! on the certificates below them: name constraints (§4.2.1.10) and certificate
//! policies, with their mappings, constraints, and `inhibitAnyPolicy` (§4.2.1.4,
//! §4.2.1.5, §4.2.1.11, §4.2.1.14). `ltv` judges each link of a chain; this judges the
//! path as a whole, from RFC 5280's initial policy set {anyPolicy} with all three
//! initial policy flags false.

use std::borrow::Cow;
use std::net::{Ipv4Addr, Ipv6Addr};

use const_oid::db::{rfc3280, rfc5280};
use const_oid::{AssociatedOid, ObjectIdentifier};
use der::{Any, Decode, Tag, Tagged};
use x509_cert::Certificate;
use x509_cert::attr::AttributeTypeAndValue;
use x509_cert::ext::pkix::certpolicy::PolicyInformation;
use x509_cert::ext::pkix::constraints::name::GeneralSubtree;
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{
    CertificatePolicies, InhibitAnyPolicy, NameConstraints, PolicyConstraints, PolicyMapping,
    PolicyMappings, SubjectAltName,
};
use x509_cert::name::{Name, RelativeDistinguishedName};

use crate::pkcs7::human_friendly;

/// Whether `cert` names itself as its issuer, which RFC 5280 calls self-issued.
pub fn self_issued(cert: &Certificate) -> bool {
    cert.tbs_certificate.subject == cert.tbs_certificate.issuer
}

/// Why `path`, the certificates below a trust anchor with the leaf first, breaks a name
/// constraint or a policy requirement one of its certificate authorities sets (RFC 5280
/// §6.1.3 (b)-(f), §6.1.4 (a), (b), (g)-(j), §6.1.5 (a), (b), (g)). The anchor's own
/// extensions constrain nothing.
pub fn constrain(path: &[Certificate]) -> Result<(), String> {
    let n = path.len();
    let mut names = Names::default();
    let mut tree = Tree::Live(vec![vec![Node::new(rfc5280::ANY_POLICY, 0)]]);
    let mut explicit = Countdown::new(n);
    let mut mapping = Countdown::new(n);
    let mut any_policy = Countdown::new(n);
    for (index, cert) in path.iter().rev().enumerate() {
        let last = index + 1 == n;
        let issued_self = self_issued(cert);
        if !issued_self || last {
            names.admit(cert)?;
        }
        let policies = extension::<CertificatePolicies>(cert, "certificate policies")?;
        if let Tree::Live(levels) = &mut tree {
            let lost = match &policies {
                None => Some(Lost::NoPolicy(cert)),
                Some(CertificatePolicies(policies)) => {
                    let claims_any = policies
                        .iter()
                        .any(|policy| policy.policy_identifier == rfc5280::ANY_POLICY);
                    // RFC 5280 §6.1.3 (d)(2): the anyPolicy of a self-issued certificate
                    // above the leaf counts whatever inhibitAnyPolicy says.
                    let inhibited_by = any_policy
                        .imposed()
                        .filter(|_| claims_any && !(issued_self && !last));
                    let any = claims_any && inhibited_by.is_none();
                    (!grow(levels, policies, any))
                        .then_some(Lost::NoneAccepted { cert, inhibited_by })
                }
            };
            if let Some(lost) = lost {
                tree = Tree::Null(lost);
            }
        }
        if let Some(by) = explicit.imposed()
            && let Tree::Null(lost) = &tree
        {
            return Err(refusal(lost, by));
        }
        if last {
            break;
        }
        if let Some(PolicyMappings(mappings)) =
            extension::<PolicyMappings>(cert, "policy mappings")?
        {
            if mappings.iter().any(|mapping| {
                mapping.issuer_domain_policy == rfc5280::ANY_POLICY
                    || mapping.subject_domain_policy == rfc5280::ANY_POLICY
            }) {
                return Err(format!(
                    "{} maps anyPolicy, which RFC 5280 section 6.1.4 forbids",
                    name(cert)
                ));
            }
            if let Tree::Live(levels) = &mut tree {
                match mapping.imposed() {
                    None => map(levels, &mappings),
                    Some(by) => {
                        if !unmap(levels, &mappings) {
                            tree = Tree::Null(Lost::MappingInhibited { cert, by });
                        }
                    }
                }
            }
        }
        if let Some(constraints) = extension::<NameConstraints>(cert, "name constraints")? {
            names.narrow(cert, constraints);
        }
        if !issued_self {
            explicit.tick();
            mapping.tick();
            any_policy.tick();
        }
        if let Some(constraints) = extension::<PolicyConstraints>(cert, "policy constraints")? {
            if let Some(skip) = constraints.require_explicit_policy {
                explicit.shorten(skip, cert);
            }
            if let Some(skip) = constraints.inhibit_policy_mapping {
                mapping.shorten(skip, cert);
            }
        }
        if let Some(InhibitAnyPolicy(skip)) =
            extension::<InhibitAnyPolicy>(cert, "inhibitAnyPolicy")?
        {
            any_policy.shorten(skip, cert);
        }
    }
    let Some(leaf) = path.first() else {
        return Ok(());
    };
    explicit.tick();
    let constraints = extension::<PolicyConstraints>(leaf, "policy constraints")?;
    if constraints.and_then(|constraints| constraints.require_explicit_policy) == Some(0) {
        explicit.shorten(0, leaf);
    }
    match (&tree, explicit.imposed()) {
        (Tree::Null(lost), Some(by)) => Err(refusal(lost, by)),
        _ => Ok(()),
    }
}

/// The extension `T` of `cert`, which errors call `what`.
fn extension<'c, T: Decode<'c> + AssociatedOid>(
    cert: &'c Certificate,
    what: &str,
) -> Result<Option<T>, String> {
    cert.tbs_certificate
        .get::<T>()
        .map(|found| found.map(|(_, value)| value))
        .map_err(|e| format!("{}: its {what} cannot be read: {e}", name(cert)))
}

fn name(cert: &Certificate) -> String {
    human_friendly(&cert.tbs_certificate.subject)
}

/// One of RFC 5280's policy countdowns (`explicit_policy`, `policy_mapping`,
/// `inhibit_anyPolicy`): how many more certificates come before its rule applies, and the
/// certificate whose constraint last brought it forward. Only a constraint brings it to
/// zero: a path of `n` certificates ticks it at most `n` times.
struct Countdown<'a> {
    left: usize,
    by: Option<&'a Certificate>,
}

impl<'a> Countdown<'a> {
    /// Past the end of a path of `n` certificates (RFC 5280 §6.1.2 (d)-(f)).
    fn new(n: usize) -> Self {
        Countdown {
            left: n + 1,
            by: None,
        }
    }

    /// One certificate on (RFC 5280 §6.1.4 (h), §6.1.5 (a)).
    fn tick(&mut self) {
        self.left = self.left.saturating_sub(1);
    }

    /// The constraint of `cert` lets only `skip` more certificates come first (RFC 5280
    /// §6.1.4 (i), (j), §6.1.5 (b)).
    fn shorten(&mut self, skip: u32, cert: &'a Certificate) {
        let skip = usize::try_from(skip).unwrap_or(usize::MAX);
        if skip < self.left {
            self.left = skip;
            self.by = Some(cert);
        }
    }

    /// The certificate whose constraint put the rule in force, once it applies.
    fn imposed(&self) -> Option<&'a Certificate> {
        if self.left == 0 { self.by } else { None }
    }
}

/// RFC 5280's `valid_policy_tree`.
enum Tree<'a> {
    /// The nodes level by level, the root's level first.
    Live(Vec<Vec<Node>>),
    /// NULL, and why.
    Null(Lost<'a>),
}

/// A node of the `valid_policy_tree`; its qualifiers decide nothing here.
struct Node {
    /// `valid_policy`.
    policy: ObjectIdentifier,
    /// `expected_policy_set`.
    expected: Vec<ObjectIdentifier>,
    /// Its parent's place in the level above.
    parent: usize,
    /// False once deleted; a deleted node keeps its place so the places below hold.
    live: bool,
}

impl Node {
    fn new(policy: ObjectIdentifier, parent: usize) -> Node {
        Node {
            policy,
            expected: vec![policy],
            parent,
            live: true,
        }
    }
}

/// How the `valid_policy_tree` went NULL.
enum Lost<'a> {
    /// The certificate names no certificate policy (RFC 5280 §6.1.3 (e)).
    NoPolicy(&'a Certificate),
    /// None of the certificate's policies is one the certificates above it accept; its
    /// anyPolicy did not count when `inhibited_by`'s inhibitAnyPolicy ruled it out.
    NoneAccepted {
        cert: &'a Certificate,
        inhibited_by: Option<&'a Certificate>,
    },
    /// The policy constraints of `by` inhibit the policy mappings of the certificate.
    MappingInhibited {
        cert: &'a Certificate,
        by: &'a Certificate,
    },
}

/// The error for a path whose policy tree went NULL as `lost` says, once the policy
/// constraints of `by` require an explicit policy (RFC 5280 §6.1.3 (f), §6.1.5 (g)).
fn refusal(lost: &Lost<'_>, by: &Certificate) -> String {
    let why = match *lost {
        Lost::NoPolicy(cert) => format!("{} names no certificate policy", name(cert)),
        Lost::NoneAccepted {
            cert,
            inhibited_by: None,
        } => format!(
            "{} names no certificate policy the certificates above it accept",
            name(cert)
        ),
        Lost::NoneAccepted {
            cert,
            inhibited_by: Some(inhibitor),
        } => format!(
            "{} names no certificate policy the certificates above it accept, and the inhibitAnyPolicy of {} keeps its anyPolicy from counting",
            name(cert),
            name(inhibitor)
        ),
        Lost::MappingInhibited {
            cert,
            by: inhibitor,
        } => format!(
            "{} maps the policies of its issuer, which the policy constraints of {} inhibit",
            name(cert),
            name(inhibitor)
        ),
    };
    format!(
        "{why}, yet the policy constraints of {} require an explicit policy (RFC 5280 section 6.1)",
        name(by)
    )
}

/// RFC 5280 §6.1.3 (d): the level `policies` grow below the deepest one, `any` when the
/// certificate's anyPolicy counts, then the prune; false once the tree is NULL.
fn grow(levels: &mut Vec<Vec<Node>>, policies: &[PolicyInformation], any: bool) -> bool {
    let Some(parents) = levels.last() else {
        return false;
    };
    let mut level: Vec<Node> = Vec::new();
    let named = policies
        .iter()
        .map(|policy| policy.policy_identifier)
        .filter(|policy| *policy != rfc5280::ANY_POLICY);
    for policy in named {
        let before = level.len();
        for (place, parent) in parents.iter().enumerate() {
            if parent.live && parent.expected.contains(&policy) {
                level.push(Node::new(policy, place));
            }
        }
        if level.len() == before {
            let any_parent = parents
                .iter()
                .position(|parent| parent.live && parent.policy == rfc5280::ANY_POLICY);
            level.extend(any_parent.map(|place| Node::new(policy, place)));
        }
    }
    if any {
        for (place, parent) in parents.iter().enumerate().filter(|(_, parent)| parent.live) {
            for &expected in &parent.expected {
                if !level
                    .iter()
                    .any(|node| node.parent == place && node.policy == expected)
                {
                    level.push(Node::new(expected, place));
                }
            }
        }
    }
    levels.push(level);
    prune(levels)
}

/// RFC 5280 §6.1.4 (b)(1): the deepest level takes `mappings`.
fn map(levels: &mut [Vec<Node>], mappings: &[PolicyMapping]) {
    let Some(level) = levels.last_mut() else {
        return;
    };
    for (place, mapping) in mappings.iter().enumerate() {
        let issuer_policy = mapping.issuer_domain_policy;
        let seen = mappings[..place]
            .iter()
            .any(|earlier| earlier.issuer_domain_policy == issuer_policy);
        if seen {
            continue;
        }
        let subjects: Vec<ObjectIdentifier> = mappings[place..]
            .iter()
            .filter(|later| later.issuer_domain_policy == issuer_policy)
            .map(|later| later.subject_domain_policy)
            .collect();
        let mut matched = false;
        for node in level
            .iter_mut()
            .filter(|node| node.live && node.policy == issuer_policy)
        {
            node.expected.clone_from(&subjects);
            matched = true;
        }
        if !matched
            && let Some(parent) = level
                .iter()
                .find(|node| node.live && node.policy == rfc5280::ANY_POLICY)
                .map(|any| any.parent)
        {
            level.push(Node {
                policy: issuer_policy,
                expected: subjects,
                parent,
                live: true,
            });
        }
    }
}

/// RFC 5280 §6.1.4 (b)(2), for when policy mapping is inhibited: the deepest level loses
/// the nodes of each policy `mappings` map, then the prune; false once the tree is NULL.
fn unmap(levels: &mut [Vec<Node>], mappings: &[PolicyMapping]) -> bool {
    if let Some(level) = levels.last_mut() {
        let mapped = |node: &&mut Node| {
            mappings
                .iter()
                .any(|mapping| mapping.issuer_domain_policy == node.policy)
        };
        for node in level.iter_mut().filter(mapped) {
            node.live = false;
        }
    }
    prune(levels)
}

/// Deletes each node above the deepest level that has no live child, until none is left
/// (RFC 5280 §6.1.3 (d)(3), §6.1.4 (b)(2)(ii)); false once the root goes and the tree is
/// NULL.
fn prune(levels: &mut [Vec<Node>]) -> bool {
    for depth in (1..levels.len()).rev() {
        let (above, below) = levels.split_at_mut(depth);
        if let (Some(parents), Some(children)) = (above.last_mut(), below.first()) {
            for (place, parent) in parents.iter_mut().enumerate() {
                parent.live &= children
                    .iter()
                    .any(|child| child.live && child.parent == place);
            }
        }
    }
    levels
        .first()
        .and_then(|level| level.first())
        .is_some_and(|root| root.live)
}

/// The name constraints in force (RFC 5280 §6.1.2 (b), (c)), each with the certificate
/// that set it.
#[derive(Default)]
struct Names<'a> {
    /// Each certificate's permitted subtrees: a name must fall within a subtree of its form
    /// in every set that has one, which makes the sets an intersection.
    permitted: Vec<(&'a Certificate, Vec<GeneralSubtree>)>,
    /// Each certificate's excluded subtrees: a name may fall within none.
    excluded: Vec<(&'a Certificate, Vec<GeneralSubtree>)>,
}

impl<'a> Names<'a> {
    /// RFC 5280 §6.1.4 (g): the permitted subtrees of `cert` narrow those in force, and
    /// its excluded subtrees add to theirs.
    fn narrow(&mut self, cert: &'a Certificate, constraints: NameConstraints) {
        if let Some(permitted) = constraints.permitted_subtrees {
            self.permitted.push((cert, permitted));
        }
        if let Some(excluded) = constraints.excluded_subtrees {
            self.excluded.push((cert, excluded));
        }
    }

    /// RFC 5280 §6.1.3 (b), (c): the subject of `cert`, each of its alternative names, and
    /// without those the email addresses in its subject (§4.2.1.10), each within the
    /// permitted subtrees and outside the excluded ones.
    fn admit(&self, cert: &Certificate) -> Result<(), String> {
        if self.permitted.is_empty() && self.excluded.is_empty() {
            return Ok(());
        }
        let subject = &cert.tbs_certificate.subject;
        if !subject.is_empty() {
            self.admit_one(cert, Held::Subject(subject))?;
        }
        match extension::<SubjectAltName>(cert, "subject alternative names")? {
            Some(SubjectAltName(alternatives)) => {
                for alternative in &alternatives {
                    self.admit_one(cert, Held::Alternative(alternative))?;
                }
            }
            None => {
                let emails = subject
                    .0
                    .iter()
                    .flat_map(|rdn| rdn.0.iter())
                    .filter(|attribute| attribute.oid == rfc3280::EMAIL_ADDRESS);
                for attribute in emails {
                    let address = text(&attribute.value).unwrap_or_default();
                    self.admit_one(cert, Held::SubjectEmail(&address))?;
                }
            }
        }
        Ok(())
    }

    /// RFC 5280 §6.1.3 (b), (c) for one name `cert` holds.
    fn admit_one(&self, cert: &Certificate, held: Held<'_>) -> Result<(), String> {
        let view = held.view();
        let form = view.form();
        for (by, trees) in &self.excluded {
            for tree in trees
                .iter()
                .filter(|tree| View::of(&tree.base).form() == form)
            {
                if within(view, tree).map_err(|why| why.explain(cert, by, held, form))? {
                    return Err(format!(
                        "{}: {} is excluded by the name constraints of {} (RFC 5280 section 4.2.1.10)",
                        name(cert),
                        held.describe(),
                        name(by)
                    ));
                }
            }
        }
        for (by, trees) in &self.permitted {
            let mut limited = false;
            let mut inside = false;
            for tree in trees
                .iter()
                .filter(|tree| View::of(&tree.base).form() == form)
            {
                limited = true;
                if within(view, tree).map_err(|why| why.explain(cert, by, held, form))? {
                    inside = true;
                    break;
                }
            }
            if limited && !inside {
                return Err(format!(
                    "{}: {} is outside the names the name constraints of {} permit (RFC 5280 section 4.2.1.10)",
                    name(cert),
                    held.describe(),
                    name(by)
                ));
            }
        }
        Ok(())
    }
}

/// A name a certificate holds that name constraints apply to.
#[derive(Clone, Copy)]
enum Held<'n> {
    /// Its subject.
    Subject(&'n Name),
    /// One of its subject alternative names.
    Alternative(&'n GeneralName),
    /// An `emailAddress` attribute of a subject that has no alternative names.
    SubjectEmail(&'n str),
}

impl<'n> Held<'n> {
    fn view(self) -> View<'n> {
        match self {
            Held::Subject(subject) => View::Directory(subject),
            Held::Alternative(alternative) => View::of(alternative),
            Held::SubjectEmail(address) => View::Email(address),
        }
    }

    /// How an error names it, as the certificate's.
    fn describe(self) -> String {
        match self {
            Held::Subject(_) => "its subject name".to_owned(),
            Held::SubjectEmail(address) => {
                format!("the email address {address} in its subject name")
            }
            Held::Alternative(alternative) => match View::of(alternative) {
                View::Directory(directory) => {
                    format!("its directory name {}", human_friendly(directory))
                }
                View::Email(address) => format!("its email address {address}"),
                View::Dns(host) => format!("its DNS name {host}"),
                View::Uri(uri) => format!("its URI {uri}"),
                View::Ip(address) => format!("its IP address {}", ip_text(address)),
                View::Other(form) => format!("one of its {}", form.plural()),
            },
        }
    }
}

/// A general name (RFC 5280 §4.2.1.6) in the shape constraints compare.
#[derive(Clone, Copy)]
enum View<'n> {
    Directory(&'n Name),
    Email(&'n str),
    Dns(&'n str),
    Uri(&'n str),
    Ip(&'n [u8]),
    /// A form whose constraints are not processed here.
    Other(Form),
}

impl<'n> View<'n> {
    fn of(general: &'n GeneralName) -> View<'n> {
        match general {
            GeneralName::DirectoryName(directory) => View::Directory(directory),
            GeneralName::Rfc822Name(address) => View::Email(address.as_str()),
            GeneralName::DnsName(host) => View::Dns(host.as_str()),
            GeneralName::UniformResourceIdentifier(uri) => View::Uri(uri.as_str()),
            GeneralName::IpAddress(address) => View::Ip(address.as_bytes()),
            GeneralName::OtherName(_) => View::Other(Form::Other),
            GeneralName::EdiPartyName(_) => View::Other(Form::EdiParty),
            GeneralName::RegisteredId(_) => View::Other(Form::Registered),
        }
    }

    fn form(self) -> Form {
        match self {
            View::Directory(_) => Form::Directory,
            View::Email(_) => Form::Email,
            View::Dns(_) => Form::Dns,
            View::Uri(_) => Form::Uri,
            View::Ip(_) => Form::Ip,
            View::Other(form) => form,
        }
    }
}

/// The forms of general name that name constraints tell apart.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Directory,
    Email,
    Dns,
    Uri,
    Ip,
    Other,
    EdiParty,
    Registered,
}

impl Form {
    fn plural(self) -> &'static str {
        match self {
            Form::Directory => "directory names",
            Form::Email => "email addresses",
            Form::Dns => "DNS names",
            Form::Uri => "URIs",
            Form::Ip => "IP addresses",
            Form::Other => "other names",
            Form::EdiParty => "EDI party names",
            Form::Registered => "registered IDs",
        }
    }
}

/// Why a name cannot be placed inside or outside a subtree of its form.
#[derive(Clone, Copy)]
enum Unclear {
    /// Constraints on the form are not processed here.
    Unsupported,
    /// The subtree sets a minimum or a maximum, which RFC 5280 §4.2.1.10 leaves unused.
    Bounded,
    /// The URI's host is missing or an IP address, which RFC 5280 §4.2.1.10 rejects under
    /// URI constraints.
    NoDomainHost,
    /// The subtree's IP address range is not an address and a mask of 4 or 16 bytes each.
    BadRange,
    /// The name is malformed: an email address without `@`, or an IP address of neither 4
    /// nor 16 bytes.
    BadName,
}

impl Unclear {
    fn explain(self, cert: &Certificate, by: &Certificate, held: Held<'_>, form: Form) -> String {
        let (cert, by) = (name(cert), name(by));
        match self {
            Unclear::Unsupported => format!(
                "{cert}: the name constraints of {by} limit {}, which are not processed here",
                form.plural()
            ),
            Unclear::Bounded => format!(
                "{cert}: the name constraints of {by} give {} a minimum or maximum, which is not processed here",
                form.plural()
            ),
            Unclear::NoDomainHost => format!(
                "{cert}: {} has no domain name for its host, which the name constraints of {by} on URIs require (RFC 5280 section 4.2.1.10)",
                held.describe()
            ),
            Unclear::BadRange => {
                format!("{cert}: the name constraints of {by} hold a malformed IP address range")
            }
            Unclear::BadName => format!(
                "{cert}: {} is malformed, so the name constraints of {by} cannot be checked",
                held.describe()
            ),
        }
    }
}

/// Whether `view` falls within `tree`, a subtree of the same form (RFC 5280 §4.2.1.10).
fn within(view: View<'_>, tree: &GeneralSubtree) -> Result<bool, Unclear> {
    if tree.minimum != 0 || tree.maximum.is_some() {
        return Err(Unclear::Bounded);
    }
    match (view, View::of(&tree.base)) {
        (View::Directory(directory), View::Directory(base)) => Ok(dn_within(directory, base)),
        (View::Email(address), View::Email(base)) => email_within(address, base),
        (View::Dns(host), View::Dns(base)) => Ok(dns_within(host, base)),
        (View::Uri(uri), View::Uri(base)) => uri_host(uri)
            .map(|host| host_within(host, base))
            .ok_or(Unclear::NoDomainHost),
        (View::Ip(address), View::Ip(range)) => ip_within(address, range),
        _ => Err(Unclear::Unsupported),
    }
}

/// Whether the distinguished name `directory` lies in the subtree `base` heads: `base`'s
/// relative distinguished names begin `directory`'s.
fn dn_within(directory: &Name, base: &Name) -> bool {
    base.0.len() <= directory.0.len()
        && base
            .0
            .iter()
            .zip(&directory.0)
            .all(|(base, part)| same_rdn(base, part))
}

/// Whether two relative distinguished names hold the same attributes.
fn same_rdn(a: &RelativeDistinguishedName, b: &RelativeDistinguishedName) -> bool {
    a.0.len() == b.0.len() && a.0.iter().all(|x| b.0.iter().any(|y| same_attribute(x, y)))
}

/// Whether two attributes are of one type and hold one value, directory strings compared
/// as RFC 5280 §7.1 asks: case and insignificant spaces aside.
fn same_attribute(a: &AttributeTypeAndValue, b: &AttributeTypeAndValue) -> bool {
    a.oid == b.oid
        && match (text(&a.value), text(&b.value)) {
            (Some(x), Some(y)) => folded(&x).eq(folded(&y)),
            _ => a.value == b.value,
        }
}

/// The text of a directory string or an IA5 string, or `None` for any other value.
fn text(value: &Any) -> Option<Cow<'_, str>> {
    match value.tag() {
        Tag::Utf8String
        | Tag::PrintableString
        | Tag::Ia5String
        | Tag::VisibleString
        | Tag::TeletexString
        | Tag::NumericString => std::str::from_utf8(value.value()).ok().map(Cow::Borrowed),
        Tag::BmpString => {
            let (pairs, odd) = value.value().as_chunks::<2>();
            if !odd.is_empty() {
                return None;
            }
            let units = pairs.iter().map(|pair| u16::from_be_bytes(*pair));
            char::decode_utf16(units)
                .collect::<Result<String, _>>()
                .ok()
                .map(Cow::Owned)
        }
        _ => None,
    }
}

/// `text` as RFC 4518 compares it, short of Unicode normalization: letters in lower case,
/// spaces at either end dropped, and each inner run of spaces one space.
fn folded(text: &str) -> impl Iterator<Item = char> + '_ {
    text.split_whitespace()
        .enumerate()
        .flat_map(|(place, word)| {
            (place > 0)
                .then_some(' ')
                .into_iter()
                .chain(word.chars().flat_map(char::to_lowercase))
        })
}

/// Whether the email address `address` falls under `base`: that mailbox, any mailbox at
/// the host `base`, or with a leading period any mailbox in the domain `base`.
fn email_within(address: &str, base: &str) -> Result<bool, Unclear> {
    let (local, host) = address.rsplit_once('@').ok_or(Unclear::BadName)?;
    Ok(match base.rsplit_once('@') {
        Some((base_local, base_host)) => {
            local == base_local && host.eq_ignore_ascii_case(base_host)
        }
        None => host_within(host, base),
    })
}

/// Whether `host` is the host `base`, or with a leading period a host in the domain
/// `base`, as URI and email constraints read it.
fn host_within(host: &str, base: &str) -> bool {
    if base.starts_with('.') {
        strip_suffix_ignoring_case(host, base).is_some_and(|rest| !rest.is_empty())
    } else {
        host.eq_ignore_ascii_case(base)
    }
}

/// Whether the DNS name `host` is `base` with zero or more labels added on its left: an
/// empty `base` takes every name, and one with a leading period only the names below it.
fn dns_within(host: &str, base: &str) -> bool {
    strip_suffix_ignoring_case(host, base).is_some_and(|rest| {
        base.is_empty()
            || if base.starts_with('.') {
                !rest.is_empty()
            } else {
                rest.is_empty() || rest.ends_with('.')
            }
    })
}

/// `text` without `suffix`, compared ignoring ASCII case.
fn strip_suffix_ignoring_case<'t>(text: &'t str, suffix: &str) -> Option<&'t str> {
    let split = text.len().checked_sub(suffix.len())?;
    let (rest, tail) = (text.get(..split)?, text.get(split..)?);
    tail.eq_ignore_ascii_case(suffix).then_some(rest)
}

/// The host of `uri` when its authority names one as a domain name rather than an IP
/// address.
fn uri_host(uri: &str) -> Option<&str> {
    let (_, rest) = uri.split_once(':')?;
    let authority = rest.strip_prefix("//")?.split(['/', '?', '#']).next()?;
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host_port
        .split_once(':')
        .map_or(host_port, |(host, _)| host);
    let numeric = host.starts_with('[') || host.parse::<Ipv4Addr>().is_ok();
    (!host.is_empty() && !numeric).then_some(host)
}

/// Whether the IP address `address` lies in `range`, an address and a mask of one family.
fn ip_within(address: &[u8], range: &[u8]) -> Result<bool, Unclear> {
    if range.len() != 8 && range.len() != 32 {
        return Err(Unclear::BadRange);
    }
    if address.len() != 4 && address.len() != 16 {
        return Err(Unclear::BadName);
    }
    let (network, mask) = range.split_at(range.len() / 2);
    Ok(address.len() == network.len()
        && address
            .iter()
            .zip(network)
            .zip(mask)
            .all(|((byte, network), mask)| byte & mask == network & mask))
}

fn ip_text(address: &[u8]) -> String {
    if let Ok(octets) = <[u8; 4]>::try_from(address) {
        Ipv4Addr::from(octets).to_string()
    } else if let Ok(octets) = <[u8; 16]>::try_from(address) {
        Ipv6Addr::from(octets).to_string()
    } else {
        format!("{address:02x?}")
    }
}
