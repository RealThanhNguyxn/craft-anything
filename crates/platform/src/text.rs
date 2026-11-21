//! Filling `{name}` placeholders in text, shared by the UI's translations and the builder's steps.

/// Fill `{name}` placeholders in one pass over `template`, so an inserted value stays literal even
/// when it looks like a placeholder. Unknown names and stray braces are kept; the first duplicate
/// name wins.
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(args.iter().fold(template.len(), |n, (_, v)| n.saturating_add(v.len())));
    let mut rest = template;
    while let Some((before, after)) = rest.split_once('{') {
        out.push_str(before);
        // The name runs to the next brace; a `{` first means this one opens no placeholder.
        let filled = after.find(['{', '}']).and_then(|end| {
            let (name, tail) = after.split_at_checked(end)?;
            let tail = tail.strip_prefix('}')?;
            Some((args.iter().find(|(k, _)| *k == name)?.1, tail))
        });
        match filled {
            Some((value, tail)) => {
                out.push_str(value);
                rest = tail;
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::fill;

    /// Inserted values stay literal: a value that looks like a placeholder, its own or a later
    /// one, is never filled in turn.
    #[test]
    fn inserted_values_stay_literal() {
        let two = "Saved {saved}, using {used}.";
        assert_eq!(fill(two, &[("saved", "{used}"), ("used", "Claude")]), "Saved {used}, using Claude.");
        assert_eq!(fill(two, &[("used", "{saved}"), ("saved", "Off")]), "Saved Off, using {saved}.");
        assert_eq!(fill(two, &[("saved", "{saved}"), ("used", "{used}")]), two);
        assert_eq!(fill("{a}{a} {b}", &[("a", "{b}"), ("b", "x")]), "{b}{b} x");
        // Unknown, empty and unbalanced braces are kept as written; the first duplicate key wins.
        assert_eq!(fill("{x} {} {a {{a}} a}", &[("a", "1")]), "{x} {} {a {1} a}");
        assert_eq!(fill("{}", &[("", "empty")]), "empty");
        assert_eq!(fill("{a}", &[("a", "1"), ("a", "2")]), "1");
        assert_eq!(fill("été {nom} ✓", &[("nom", "Zoë")]), "été Zoë ✓");
        assert_eq!(fill("", &[("a", "1")]), "");
        assert_eq!(fill("no placeholders", &[("a", "1")]), "no placeholders");
    }
}
