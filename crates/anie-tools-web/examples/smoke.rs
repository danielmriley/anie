//! Manual smoke test for web_read and web_search.
//!
//! Run:
//!     cargo run --example smoke -p anie-tools-web -- search "rust async"
//!     cargo run --example smoke -p anie-tools-web -- read https://example.com
//!     cargo run --example smoke -p anie-tools-web --features headless -- \
//!         readjs https://weather.com/...
//!     cargo run --example smoke -p anie-tools-web --features headless -- \
//!         render https://example.com
//!
//! Not part of the test suite. This is a network-touching tool
//! we run by hand to verify behavior end-to-end against real
//! services (DuckDuckGo, Defuddle subprocess, Chrome).

use anie_agent::Tool;
use anie_tools_web::{WebReadTool, WebSearchTool};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "help".into());
    let target = args.next();

    match cmd.as_str() {
        "search" => {
            let query = target.ok_or("usage: smoke search <query>")?;
            let tool = WebSearchTool::new()?;
            let payload = serde_json::json!({ "query": query, "max_results": 5 });
            let result = tool
                .execute("smoke", payload, CancellationToken::new(), None)
                .await?;
            print_result(&result);
        }
        "read" => {
            let url = target.ok_or("usage: smoke read <url>")?;
            let tool = WebReadTool::new()?;
            let payload = serde_json::json!({ "url": url });
            let result = tool
                .execute("smoke", payload, CancellationToken::new(), None)
                .await?;
            print_result(&result);
        }
        "readjs" => {
            let url = target.ok_or("usage: smoke readjs <url>")?;
            let tool = WebReadTool::new()?;
            let payload = serde_json::json!({ "url": url, "javascript": true });
            let result = tool
                .execute("smoke", payload, CancellationToken::new(), None)
                .await?;
            print_result(&result);
        }
        #[cfg(feature = "headless")]
        "render" => {
            let url_str = target.ok_or("usage: smoke render <url>")?;
            let url = url::Url::parse(&url_str)?;
            let cancel = CancellationToken::new();
            let html = anie_tools_web::read::headless::render_with_chrome(
                &url,
                std::time::Duration::from_secs(30),
                &cancel,
            )
            .await?;
            // Print the full HTML so it can be redirected.
            print!("{html}");
            eprintln!("[render] {} bytes of post-DOM HTML", html.len());
        }
        #[cfg(not(feature = "headless"))]
        "render" => {
            eprintln!(
                "render requires --features headless: \
                 cargo run --example smoke -p anie-tools-web --features headless -- render <url>"
            );
            std::process::exit(2);
        }
        _ => {
            eprintln!(
                "smoke test runner\n\n\
                 commands:\n  \
                   search <query>   — run web_search against DuckDuckGo\n  \
                   read <url>       — run web_read (Defuddle, no JS)\n  \
                   readjs <url>     — run web_read with javascript=true (--features headless)\n  \
                   render <url>     — headless Chrome render only (--features headless)"
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

fn print_result(result: &anie_protocol::ToolResult) {
    use anie_protocol::ContentBlock;
    for block in &result.content {
        match block {
            ContentBlock::Text { text } => println!("{text}"),
            other => println!("[non-text block: {:?}]", std::mem::discriminant(other)),
        }
    }
    println!("---\ndetails: {}", result.details);
}
