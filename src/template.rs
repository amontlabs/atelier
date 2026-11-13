//! The Mustache subset atelier needs (DESIGN §2.7), used for task templates and for the rule
//! texts: `{{name}}` inserts a value; `{{#name}}…{{/name}}` keeps its content only when `name`
//! has a non-empty value. Nothing else: no escaping, partials, lists, inverted sections or
//! dotted names.
//!
//! Why not a crate: ramhorns and the other maintained Mustache crates render typed Rust structs
//! (derive macros) and implement the whole specification. Here the fields come from a template's
//! header at run time, and anything beyond these two tags must be an error rather than a feature.
//! This is about 150 lines, with the one specification rule that matters for Markdown: a line
//! holding only a section tag disappears with its line break.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Text(String),
    Var(String),
    Section(String, Vec<Node>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    nodes: Vec<Node>,
}

/// Field and flag names.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

impl Template {
    pub fn parse(src: &str) -> Result<Template, String> {
        let src = strip_standalone(src);
        let mut stack: Vec<(String, Vec<Node>)> = vec![(String::new(), Vec::new())];
        let mut rest = src.as_str();
        while !rest.is_empty() {
            let Some(open) = rest.find("{{") else {
                push(&mut stack, Node::Text(rest.to_owned()));
                break;
            };
            if open > 0 {
                push(&mut stack, Node::Text(rest[..open].to_owned()));
            }
            let after = &rest[open + 2..];
            let close = after
                .find("}}")
                .ok_or_else(|| "a `{{` is never closed".to_owned())?;
            let tag = after[..close].trim();
            rest = &after[close + 2..];
            match tag.chars().next() {
                Some('#') => {
                    let name = check_name(&tag[1..])?;
                    stack.push((name, Vec::new()));
                }
                Some('/') => {
                    let name = check_name(&tag[1..])?;
                    if stack.len() == 1 || stack.last().is_some_and(|(n, _)| *n != name) {
                        return Err(format!("`{{{{/{name}}}}}` closes nothing that is open"));
                    }
                    let (_, nodes) = stack.pop().expect("checked above");
                    push(&mut stack, Node::Section(name, nodes));
                }
                _ => {
                    let name = check_name(tag)?;
                    push(&mut stack, Node::Var(name));
                }
            }
        }
        if stack.len() != 1 {
            return Err(format!("`{{{{#{}}}}}` is never closed", stack[1].0));
        }
        Ok(Template {
            nodes: stack.pop().expect("one frame").1,
        })
    }

    /// Every name the template uses, in sections or as values.
    pub fn names(&self) -> BTreeSet<String> {
        fn walk(nodes: &[Node], out: &mut BTreeSet<String>) {
            for n in nodes {
                match n {
                    Node::Text(_) => {}
                    Node::Var(v) => {
                        out.insert(v.clone());
                    }
                    Node::Section(v, inner) => {
                        out.insert(v.clone());
                        walk(inner, out);
                    }
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(&self.nodes, &mut out);
        out
    }

    /// Fills the template. A name with no value, or an empty one, inserts nothing and keeps no
    /// section.
    pub fn render(&self, values: &BTreeMap<String, String>) -> String {
        fn walk(nodes: &[Node], values: &BTreeMap<String, String>, out: &mut String) {
            for n in nodes {
                match n {
                    Node::Text(t) => out.push_str(t),
                    Node::Var(v) => {
                        if let Some(x) = values.get(v) {
                            out.push_str(x);
                        }
                    }
                    Node::Section(v, inner) => {
                        if values.get(v).is_some_and(|x| !x.is_empty()) {
                            walk(inner, values, out);
                        }
                    }
                }
            }
        }
        let mut out = String::new();
        walk(&self.nodes, values, &mut out);
        out
    }
}

fn push(stack: &mut [(String, Vec<Node>)], node: Node) {
    stack.last_mut().expect("stack never empty").1.push(node);
}

fn check_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if valid_name(name) {
        Ok(name.to_owned())
    } else {
        Err(format!(
            "{name:?} is not a field name (a-z, 0-9, `_`, `-`; `{{{{#name}}}}` and `{{{{/name}}}}` are the only other tags)"
        ))
    }
}

/// A line holding only one section tag disappears, line break included.
fn strip_standalone(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let t = line.trim();
        let tag_only = (t.starts_with("{{#") || t.starts_with("{{/"))
            && t.ends_with("}}")
            && t.matches("{{").count() == 1;
        if tag_only {
            out.push_str(t);
        } else {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn values_and_sections() {
        let t = Template::parse("a {{x}} b\n{{#n}}\nnote {{n}}\n{{/n}}\nend\n").unwrap();
        assert_eq!(t.render(&vals(&[("x", "1")])), "a 1 b\nend\n");
        assert_eq!(
            t.render(&vals(&[("x", "1"), ("n", "hi")])),
            "a 1 b\nnote hi\nend\n"
        );
        assert_eq!(t.render(&vals(&[("n", "")])), "a  b\nend\n");
        assert_eq!(
            t.names(),
            ["n", "x"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn inline_sections_and_nesting() {
        let t = Template::parse("a{{#p}}, b{{#q}}!{{/q}}{{/p}}.").unwrap();
        assert_eq!(t.render(&vals(&[])), "a.");
        assert_eq!(t.render(&vals(&[("p", "y")])), "a, b.");
        assert_eq!(t.render(&vals(&[("p", "y"), ("q", "y")])), "a, b!.");
    }

    #[test]
    fn values_are_not_interpreted() {
        let t = Template::parse("{{x}}").unwrap();
        assert_eq!(
            t.render(&vals(&[("x", "{{y}} <b> {{#z}}")])),
            "{{y}} <b> {{#z}}"
        );
    }

    #[test]
    fn malformed_templates_are_errors() {
        for bad in [
            "{{x",
            "{{#a}}x",
            "{{/a}}",
            "{{#a}}{{/b}}",
            "{{^a}}x{{/a}}",
            "{{{x}}}",
            "{{a.b}}",
            "{{>p}}",
            "{{}}",
            "{{X}}",
        ] {
            assert!(Template::parse(bad).is_err(), "{bad}");
        }
    }
}
