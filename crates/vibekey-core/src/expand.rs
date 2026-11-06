//! Snippet template expansion.
//!
//! Supported tokens: {date}, {time}, {datetime}, {clipboard}, {user}, {host}.
//! A doubled opening brace produces a literal brace, so "{{date}}" renders as
//! "{date}". Unknown tokens are left untouched.

use chrono::{DateTime, Local};

/// Context needed to expand a snippet text.
pub struct ExpandContext<'a> {
    pub now: DateTime<Local>,
    pub clipboard: Option<&'a str>,
    pub user: &'a str,
    pub host: &'a str,
}

impl<'a> ExpandContext<'a> {
    /// Context using the current time and no clipboard/user/host information.
    pub fn now() -> Self {
        Self {
            now: Local::now(),
            clipboard: None,
            user: "",
            host: "",
        }
    }
}

/// Expand a snippet template.
pub fn expand(template: &str, ctx: &ExpandContext<'_>) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '{' if i + 1 < chars.len() && chars[i + 1] == '{' => {
                out.push('{');
                i += 2;
            }
            '}' if i + 1 < chars.len() && chars[i + 1] == '}' => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let mut j = i + 1;
                let mut closed = false;
                while j < chars.len() {
                    match chars[j] {
                        '}' => {
                            closed = true;
                            break;
                        }
                        '{' => break,
                        _ => j += 1,
                    }
                }
                if closed {
                    let name: String = chars[i + 1..j].iter().collect();
                    match name.trim().to_ascii_lowercase().as_str() {
                        "date" => out.push_str(&ctx.now.format("%Y-%m-%d").to_string()),
                        "time" => out.push_str(&ctx.now.format("%H:%M").to_string()),
                        "datetime" => out.push_str(&ctx.now.format("%Y-%m-%d %H:%M").to_string()),
                        "clipboard" => out.push_str(ctx.clipboard.unwrap_or("")),
                        "user" => out.push_str(ctx.user),
                        "host" => out.push_str(ctx.host),
                        _ => {
                            out.push('{');
                            out.push_str(&name);
                            out.push('}');
                        }
                    }
                    i = j + 1;
                } else {
                    out.push('{');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx<'a>(clipboard: Option<&'a str>) -> ExpandContext<'a> {
        ExpandContext {
            now: Local
                .with_ymd_and_hms(2026, 11, 6, 9, 7, 0)
                .single()
                .expect("valid local time"),
            clipboard,
            user: "zzq",
            host: "desktop",
        }
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(expand("hello world", &ctx(None)), "hello world");
    }

    #[test]
    fn expands_date_and_time() {
        assert_eq!(expand("{date}", &ctx(None)), "2026-11-06");
        assert_eq!(expand("{time}", &ctx(None)), "09:07");
        assert_eq!(expand("{datetime}", &ctx(None)), "2026-11-06 09:07");
    }

    #[test]
    fn expands_clipboard_user_and_host() {
        assert_eq!(expand("[{clipboard}]", &ctx(Some("abc"))), "[abc]");
        assert_eq!(expand("[{clipboard}]", &ctx(None)), "[]");
        assert_eq!(expand("{user}@{host}", &ctx(None)), "zzq@desktop");
    }

    #[test]
    fn unknown_tokens_and_braces_are_preserved() {
        assert_eq!(expand("{unknown}", &ctx(None)), "{unknown}");
        assert_eq!(expand("{{date}}", &ctx(None)), "{date}");
        assert_eq!(expand("a { b", &ctx(None)), "a { b");
        assert_eq!(expand("100% {", &ctx(None)), "100% {");
        assert_eq!(expand("{{}}", &ctx(None)), "{}");
    }

    #[test]
    fn mixed_text_round_trip() {
        let text = "Hi {user}, it is {date} {time} and the clipboard says {clipboard}.";
        assert_eq!(
            expand(text, &ctx(Some("x"))),
            "Hi zzq, it is 2026-11-06 09:07 and the clipboard says x."
        );
    }
}
