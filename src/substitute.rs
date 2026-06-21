//! Dockerfile-style variable substitution.
//!
//! Supports `$VAR`, `${VAR}`, `${VAR:-default}`, and `${VAR:+replacement}`.
//! Only `\$` is treated as an escape sequence (producing a literal `$`).
//! All other backslash sequences are passed through unchanged, so shell
//! constructs like `\n`, `\t`, or line-continuation `\` are not disturbed.
use crate::state::VarMap;
use anyhow::Result;

pub fn substitute(s: &str, vars: &VarMap) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some((_, '$')) => {
                    // \$ → literal $
                    out.push('$');
                    chars.next();
                }
                _ => {
                    // Pass the backslash through unchanged
                    out.push('\\');
                }
            }
        } else if ch == '$' {
            match chars.peek() {
                Some((_, '{')) => {
                    chars.next(); // consume '{'
                    let mut inner = String::new();
                    let mut closed = false;
                    for (_, c) in chars.by_ref() {
                        if c == '}' {
                            closed = true;
                            break;
                        }
                        inner.push(c);
                    }
                    if !closed {
                        anyhow::bail!(
                            "Missing closing brace in variable substitution: ${{{}",
                            inner
                        );
                    }
                    if let Some(pos) = inner.find(":-") {
                        let name = &inner[..pos];
                        let default = &inner[pos + 2..];
                        let v = vars.get(name);
                        out.push_str(if v.is_empty() { default } else { v });
                    } else if let Some(pos) = inner.find(":+") {
                        let name = &inner[..pos];
                        let replacement = &inner[pos + 2..];
                        if !vars.get(name).is_empty() {
                            out.push_str(replacement);
                        }
                    } else {
                        out.push_str(vars.get(&inner));
                    }
                }
                Some((_, c)) if c.is_ascii_alphanumeric() || *c == '_' => {
                    let mut name = String::new();
                    while let Some((_, c)) = chars.peek() {
                        if c.is_ascii_alphanumeric() || *c == '_' {
                            name.push(*c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    out.push_str(vars.get(&name));
                    // unset variable expands to empty string (Docker behaviour)
                }
                _ => {
                    out.push('$');
                }
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> VarMap {
        let mut v = VarMap::default();
        for (k, val) in pairs {
            v.env.insert(k.to_string(), val.to_string());
        }
        v
    }

    #[test]
    fn test_basic_substitution() {
        let v = vars(&[("Y", "24")]);
        assert_eq!(
            substitute("X is ${X} and Y is ${Y}", &v).unwrap(),
            "X is  and Y is 24"
        );
    }

    #[test]
    fn test_bare_variable() {
        let v = vars(&[("FOO", "bar")]);
        assert_eq!(substitute("Hello $FOO!", &v).unwrap(), "Hello bar!");
    }

    #[test]
    fn test_unset_expands_to_empty() {
        let v = vars(&[]);
        assert_eq!(substitute("${UNSET}", &v).unwrap(), "");
        assert_eq!(substitute("$UNSET", &v).unwrap(), "");
    }

    #[test]
    fn test_default_value() {
        let v = vars(&[]);
        assert_eq!(substitute("${X:-fallback}", &v).unwrap(), "fallback");
    }

    #[test]
    fn test_default_value_not_used_when_set() {
        let v = vars(&[("X", "42")]);
        assert_eq!(substitute("${X:-fallback}", &v).unwrap(), "42");
    }

    #[test]
    fn test_replacement_value() {
        let v = vars(&[("X", "42")]);
        assert_eq!(substitute("${X:+isset}", &v).unwrap(), "isset");
    }

    #[test]
    fn test_replacement_value_empty_when_unset() {
        let v = vars(&[]);
        assert_eq!(substitute("${X:+isset}", &v).unwrap(), "");
    }

    #[test]
    fn test_escaped_dollar() {
        let v = vars(&[("X", "42")]);
        assert_eq!(substitute(r"\$X", &v).unwrap(), "$X");
    }

    #[test]
    fn test_backslash_passthrough() {
        let v = vars(&[]);
        // \n and \\ should be passed through unchanged
        assert_eq!(substitute(r"printf '%s\n'", &v).unwrap(), r"printf '%s\n'");
    }

    #[test]
    fn test_missing_closing_brace() {
        let v = vars(&[]);
        assert!(substitute("${X", &v).is_err());
    }
}
