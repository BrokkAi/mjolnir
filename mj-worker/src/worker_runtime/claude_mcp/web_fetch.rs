use super::*;
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
        if format == "markdown" {
            htmd::HtmlToMarkdown::builder()
                .skip_tags(vec!["script", "style", "noscript", "template"])
                .build()
                .convert(&raw)?
        } else {
            html2text::from_read_with_decorator(
                raw.as_bytes(),
                usize::MAX,
                html2text::render::TrivialDecorator::new(),
            )?
        }
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
            for n in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut req = vec![0; 4096];
                use tokio::io::AsyncReadExt;
                assert!(stream.read(&mut req).await.unwrap() > 0);
                let body = "<html><script src='ignored'></script><h1>hé😀</h1><p><a data-href='https://wrong.example' href='https://example.com'>link &amp; more</a><code>x</code></p><script>bad; if (a < b) { console.log(1); }</script><p>after x < y</p><a title=\"a > b\" href=\"/docs\">Docs</a></html>";
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
        assert!(content.contains("[Docs](/docs \"a > b\")"));
        assert!(!content.contains("b\" href="));
        assert!(content.contains("x < y"));
        let text = fetch(json!({"url":format!("http://{addr}/final"),"format":"text"}))
            .await
            .unwrap();
        let text = text["content"].as_str().unwrap();
        assert!(text.contains("hé😀") && text.contains("Docs") && text.contains("x < y"));
        assert!(!text.contains("[Docs]") && !text.contains("console.log"));
        let raw = fetch(json!({"url":format!("http://{addr}/final"),"format":"raw"}))
            .await
            .unwrap();
        assert!(raw["content"].as_str().unwrap().contains("<script>bad;"));
        server.await.unwrap();
        assert!(fetch(json!({"url":"file:///etc/passwd"})).await.is_err());
    }
}
