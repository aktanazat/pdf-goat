//! The line matcher and unified diff used by Python's difflib. No junk
//! predicate; optional autojunk only for unified_diff (as in Python).

use std::collections::HashMap;

#[derive(Clone, Copy)]
struct Match {
    a: usize,
    b: usize,
    size: usize,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Equal,
    Replace,
    Delete,
    Insert,
}
#[derive(Clone, Copy)]
struct Op {
    tag: Tag,
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
}

pub(crate) struct Matcher<'a> {
    b: &'a [String],
    positions: HashMap<&'a str, Vec<usize>>,
    counts: HashMap<&'a str, usize>,
}

impl<'a> Matcher<'a> {
    pub fn new(b: &'a [String], autojunk: bool) -> Self {
        let mut positions: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, line) in b.iter().enumerate() {
            positions.entry(line).or_default().push(i);
        }
        let counts = positions
            .iter()
            .map(|(&key, values)| (key, values.len()))
            .collect();
        if autojunk && b.len() >= 200 {
            let threshold = b.len() / 100 + 1;
            positions.retain(|_, values| values.len() <= threshold);
        }
        Self {
            b,
            positions,
            counts,
        }
    }
    pub fn real_quick_ratio(&self, a: &[String]) -> f64 {
        ratio(a.len().min(self.b.len()), a.len() + self.b.len())
    }
    pub fn quick_ratio(&self, a: &[String]) -> f64 {
        let mut used = HashMap::new();
        let mut matches = 0;
        for line in a {
            let count = used.entry(line.as_str()).or_insert(0);
            if *count < self.counts.get(line.as_str()).copied().unwrap_or(0) {
                matches += 1;
            }
            *count += 1;
        }
        ratio(matches, a.len() + self.b.len())
    }
    pub fn ratio(&self, a: &[String]) -> f64 {
        ratio(
            self.matches(a).iter().map(|m| m.size).sum(),
            a.len() + self.b.len(),
        )
    }

    fn longest(&self, a: &[String], alo: usize, ahi: usize, blo: usize, bhi: usize) -> Match {
        let mut best = Match {
            a: alo,
            b: blo,
            size: 0,
        };
        let mut prior = HashMap::new();
        for (i, line) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut lengths = HashMap::new();
            if let Some(positions) = self.positions.get(line.as_str()) {
                for &j in positions {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let size = 1 + j
                        .checked_sub(1)
                        .and_then(|j| prior.get(&j))
                        .copied()
                        .unwrap_or(0);
                    lengths.insert(j, size);
                    if size > best.size {
                        best = Match {
                            a: i + 1 - size,
                            b: j + 1 - size,
                            size,
                        };
                    }
                }
            }
            prior = lengths;
        }
        while best.a > alo && best.b > blo && a[best.a - 1] == self.b[best.b - 1] {
            best.a -= 1;
            best.b -= 1;
            best.size += 1;
        }
        while best.a + best.size < ahi
            && best.b + best.size < bhi
            && a[best.a + best.size] == self.b[best.b + best.size]
        {
            best.size += 1;
        }
        best
    }

    fn matches(&self, a: &[String]) -> Vec<Match> {
        let mut queue = vec![(0, a.len(), 0, self.b.len())];
        let mut found = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let m = self.longest(a, alo, ahi, blo, bhi);
            if m.size == 0 {
                continue;
            }
            found.push(m);
            if alo < m.a && blo < m.b {
                queue.push((alo, m.a, blo, m.b));
            }
            if m.a + m.size < ahi && m.b + m.size < bhi {
                queue.push((m.a + m.size, ahi, m.b + m.size, bhi));
            }
        }
        found.sort_by_key(|m| (m.a, m.b, m.size));
        let mut merged: Vec<Match> = Vec::new();
        for m in found {
            if let Some(last) = merged
                .last_mut()
                .filter(|last| last.a + last.size == m.a && last.b + last.size == m.b)
            {
                last.size += m.size;
            } else {
                merged.push(m);
            }
        }
        merged.push(Match {
            a: a.len(),
            b: self.b.len(),
            size: 0,
        });
        merged
    }

    fn opcodes(&self, a: &[String]) -> Vec<Op> {
        let (mut i, mut j) = (0, 0);
        let mut ops = Vec::new();
        for m in self.matches(a) {
            let tag = if i < m.a && j < m.b {
                Some(Tag::Replace)
            } else if i < m.a {
                Some(Tag::Delete)
            } else if j < m.b {
                Some(Tag::Insert)
            } else {
                None
            };
            if let Some(tag) = tag {
                ops.push(Op {
                    tag,
                    a0: i,
                    a1: m.a,
                    b0: j,
                    b1: m.b,
                });
            }
            i = m.a + m.size;
            j = m.b + m.size;
            if m.size > 0 {
                ops.push(Op {
                    tag: Tag::Equal,
                    a0: m.a,
                    a1: i,
                    b0: m.b,
                    b1: j,
                });
            }
        }
        ops
    }
}

fn ratio(matches: usize, total: usize) -> f64 {
    if total == 0 {
        1.0
    } else {
        2.0 * matches as f64 / total as f64
    }
}

fn groups(mut ops: Vec<Op>, n: usize) -> Vec<Vec<Op>> {
    if ops.is_empty() {
        return Vec::new();
    }
    if let Some(first) = ops.first_mut().filter(|op| op.tag == Tag::Equal) {
        first.a0 = first.a0.max(first.a1.saturating_sub(n));
        first.b0 = first.b0.max(first.b1.saturating_sub(n));
    }
    if let Some(last) = ops.last_mut().filter(|op| op.tag == Tag::Equal) {
        last.a1 = last.a1.min(last.a0.saturating_add(n));
        last.b1 = last.b1.min(last.b0.saturating_add(n));
    }
    let mut groups = Vec::new();
    let mut group = Vec::new();
    for mut op in ops {
        if op.tag == Tag::Equal && op.a1 - op.a0 > n.saturating_mul(2) {
            group.push(Op {
                a1: op.a0 + n,
                b1: op.b0 + n,
                ..op
            });
            groups.push(std::mem::take(&mut group));
            op.a0 = op.a1 - n;
            op.b0 = op.b1 - n;
        }
        group.push(op);
    }
    if !group.is_empty() && !(group.len() == 1 && group[0].tag == Tag::Equal) {
        groups.push(group);
    }
    groups
}

fn range(start: usize, stop: usize) -> String {
    let length = stop - start;
    if length == 1 {
        (start + 1).to_string()
    } else {
        format!("{},{length}", if length == 0 { start } else { start + 1 })
    }
}

/// `list(unified_diff(a,b,lineterm="",n=context))[2:]`.
pub(crate) fn unified(a: &[String], b: &[String], context: usize) -> Vec<String> {
    let matcher = Matcher::new(b, true);
    let mut out = Vec::new();
    for group in groups(matcher.opcodes(a), context) {
        let Some(first) = group.first() else {
            continue;
        };
        let Some(last) = group.last() else {
            continue;
        };
        out.push(format!(
            "@@ -{} +{} @@",
            range(first.a0, last.a1),
            range(first.b0, last.b1)
        ));
        for op in group {
            if op.tag == Tag::Equal {
                for line in &a[op.a0..op.a1] {
                    out.push(format!(" {line}"));
                }
            }
            if matches!(op.tag, Tag::Replace | Tag::Delete) {
                for line in &a[op.a0..op.a1] {
                    out.push(format!("-{line}"));
                }
            }
            if matches!(op.tag, Tag::Replace | Tag::Insert) {
                for line in &b[op.b0..op.b1] {
                    out.push(format!("+{line}"));
                }
            }
        }
    }
    out
}
