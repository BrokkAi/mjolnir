use super::*;
fn entities(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let limit = rest.floor_char_boundary(16.min(rest.len()));
        if let Some(end) = rest[..limit].find(';') {
            let e = &rest[1..end];
            let decoded = match e {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ => e
                    .strip_prefix("#x")
                    .or_else(|| e.strip_prefix("#X"))
                    .and_then(|v| u32::from_str_radix(v, 16).ok())
                    .or_else(|| e.strip_prefix('#').and_then(|v| v.parse().ok()))
                    .and_then(char::from_u32),
            };
            if let Some(c) = decoded {
                out.push(c);
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}
fn find_closing_tag(rest: &str, hidden: &str) -> Option<usize> {
    let mut pos = 0;
    while let Some(relative) = rest[pos..].find("</") {
        let name = pos + relative + 2;
        if rest[name..]
            .get(..hidden.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(hidden))
        {
            let mut end = name + hidden.len();
            while rest
                .as_bytes()
                .get(end)
                .is_some_and(u8::is_ascii_whitespace)
            {
                end += 1;
            }
            if rest.as_bytes().get(end) == Some(&b'>') {
                return Some(end + 1);
            }
        }
        pos = name;
    }
    None
}

fn find_tag_end(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (i, &byte) in rest.as_bytes().iter().enumerate().skip(1) {
        match quote {
            Some(q) if byte == q => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\'' | b'"') => quote = Some(byte),
            None if byte == b'>' => return Some(i),
            None => {}
        }
    }
    rest.find('>')
}

fn convert(html: &str, markdown: bool) -> String {
    let mut out = String::new();
    let mut rest = html;
    let mut skip: Option<String> = None;
    let mut links = Vec::new();
    let mut pre = false;
    while !rest.is_empty() {
        if let Some(hidden) = &skip {
            rest = find_closing_tag(rest, hidden).map_or("", |end| &rest[end..]);
            skip = None;
            continue;
        }
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |n| &rest[n + 3..]);
            continue;
        }
        if rest.starts_with('<') {
            let Some(end) = find_tag_end(rest) else {
                break;
            };
            let tag = &rest[1..end];
            let lower = tag.to_ascii_lowercase();
            let close = lower.starts_with('/');
            let name = lower
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches('/');
            rest = &rest[end + 1..];
            if ["script", "style", "noscript", "template"].contains(&name)
                && !close
                && !tag.trim_end().ends_with('/')
            {
                skip = Some(name.into());
                continue;
            }
            if markdown {
                match name {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        out.push_str("\n\n");
                        if !close {
                            out.push_str(&"#".repeat(name.as_bytes()[1] as usize - b'0' as usize));
                            out.push(' ');
                        }
                    }
                    "a" => {
                        if close {
                            if let Some(href) = links.pop() {
                                out.push_str(&format!("]({href})"));
                            }
                        } else {
                            let href = attribute(tag, "href");
                            if let Some(href) = href {
                                links.push(entities(&href));
                                out.push('[');
                            }
                        }
                    }
                    "li" => {
                        out.push('\n');
                        if !close {
                            out.push_str("- ");
                        }
                    }
                    "pre" => {
                        out.push_str("\n```\n");
                        pre = !close;
                    }
                    "code" if !pre => out.push('`'),
                    _ => {}
                }
            }
            if ["p", "div", "br", "tr", "ul", "ol"].contains(&name) {
                out.push('\n');
            }
            continue;
        }
        let end = rest.find('<').unwrap_or(rest.len());
        if skip.is_some() {
            rest = &rest[end..];
            continue;
        }
        let text = entities(&rest[..end]);
        if pre {
            out.push_str(&text);
        } else {
            let mut whitespace = out.ends_with(char::is_whitespace);
            for c in text.chars() {
                if c.is_whitespace() {
                    if !whitespace {
                        out.push(' ');
                    }
                    whitespace = true;
                } else {
                    out.push(c);
                    whitespace = false;
                }
            }
        }
        rest = &rest[end..];
    }
    let mut clean = String::new();
    let mut blank = false;
    for line in out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if blank {
                continue;
            }
            blank = true;
        } else {
            blank = false;
        }
        clean.push_str(line);
        clean.push('\n');
    }
    clean.trim().into()
}
fn attribute(tag: &str, key: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let i = lower
        .match_indices(&format!("{key}="))
        .find(|(i, _)| *i > 0 && tag.as_bytes()[i - 1].is_ascii_whitespace())?
        .0
        + key.len()
        + 1;
    let s = &tag[i..];
    if s.starts_with(['\'', '"']) {
        let q = s.chars().next()?;
        Some(s[1..].split(q).next()?.into())
    } else {
        Some(s.split_whitespace().next()?.into())
    }
}
pub(super) async fn fetch(a: Value) -> Result<Value> {
    let url = required(&a, "url")?;
    let parsed = reqwest::Url::parse(&url)?;
    anyhow::ensure!(
        ["http", "https"].contains(&parsed.scheme()),
        "URL must use HTTP or HTTPS"
    );
    let format = a["format"].as_str().unwrap_or("markdown");
    anyhow::ensure!(
        ["markdown", "text", "raw"].contains(&format),
        "invalid format"
    );
    let offset = a["offset"].as_u64().unwrap_or(0) as usize;
    let max = a["max_chars"].as_u64().unwrap_or(32768) as usize;
    anyhow::ensure!(max > 0, "max_chars must be positive");
    let started = std::time::Instant::now();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent("mjolnir-web-fetch/1.0")
        .build()?;
    let response = client.get(parsed).send().await?;
    let final_url = response.url().to_string();
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = response.bytes().await?;
    let raw = String::from_utf8_lossy(&body);
    let html = content_type.contains("html")
        || ["<!doctype", "<html"]
            .iter()
            .any(|prefix| raw.trim_start().to_ascii_lowercase().starts_with(prefix));
    let content = if html && format != "raw" {
        convert(&raw, format == "markdown")
    } else {
        raw.into_owned()
    };
    let total = content.chars().count();
    let page: String = content.chars().skip(offset).take(max).collect();
    let next = offset.saturating_add(page.chars().count());
    Ok(
        json!({"url":url,"final_url":final_url,"status":status.as_u16(),"status_text":status.canonical_reason().unwrap_or(""),"content_type":content_type,"bytes":body.len(),"duration_ms":started.elapsed().as_millis(),"format":format,"content":page,"offset":offset,"next_offset":if next<total{Some(next)}else{None},"total_chars":total,"truncated":next<total}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn claude_mcp_http_redirect_html_unicode_paging() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = mj_core::runtime::spawn(async move {
            for n in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut req = vec![0; 4096];
                use tokio::io::AsyncReadExt;
                assert!(stream.read(&mut req).await.unwrap() > 0);
                let body = "<html><script src='ignored' /><h1>hé😀</h1><p><a data-href='https://wrong.example' href='https://example.com'>link &amp; more</a><code>x</code></p><script>bad; if (a < b) { console.log(1); }</script><p>after</p><a title=\"a > b\" href=\"/docs\">Docs</a></html>";
                let reply = if n == 0 {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{addr}/final\r\nContent-Length: 0\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let result = fetch(json!({"url":format!("http://{addr}/"),"offset":2,"max_chars":3}))
            .await
            .unwrap();
        assert_eq!(result["content"], "hé😀");
        assert_eq!(result["next_offset"], 5);
        assert!(result["final_url"].as_str().unwrap().ends_with("/final"));
        let whole = fetch(json!({"url":format!("http://{addr}/final")}))
            .await
            .unwrap();
        let content = whole["content"].as_str().unwrap();
        assert!(content.contains("# hé😀"));
        assert!(content.contains("[link & more](https://example.com)"));
        assert!(content.contains("`x`"));
        assert!(!content.contains("bad"));
        assert!(!content.contains("console.log"));
        assert!(content.contains("after"));
        assert!(content.contains("[Docs](/docs)"));
        assert!(!content.contains("b\" href="));
        server.await.unwrap();
        assert!(fetch(json!({"url":"file:///etc/passwd"})).await.is_err());
    }
}
