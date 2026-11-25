//! feedpush — 自架 RSSHub 的 feed → 去重 → 可直接推 Telegram 的文字
//!
//! 與 ~/work/rss-feeds/push.py 行為一致，但編成單一執行檔（不需 Python）：
//!   feedpush                     # 正常（cron no_agent：沒有新項目就完全沒有輸出）
//!   feedpush --test              # 忽略去重，印出目前內容
//!   feedpush --feed hackernews   # 只跑指定 feed
//!   feedpush --config <path>     # 指定 feeds.json

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

const MAX_ITEMS: usize = 8;
const MAX_CHARS: usize = 3800; // Telegram 上限 4096，留安全邊界
const TIMEOUT_SECS: u64 = 25;

#[derive(Deserialize)]
struct Config {
    feeds: Vec<Feed>,
}

#[derive(Deserialize, Clone)]
struct Feed {
    id: String,
    name: String,
    route: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    keywords: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
struct Item {
    id: String,
    title: String,
    link: String,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    #[serde(default)]
    seen: Vec<String>,
    #[serde(default)]
    updated: String,
}

struct Args {
    test: bool,
    feed: Option<String>,
    config: PathBuf,
    base: String,
}

fn parse_args() -> Args {
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut args = Args {
        test: false,
        feed: None,
        config: PathBuf::from(format!("{home}/work/rss-feeds/feeds.json")),
        base: env::var("RSSHUB_BASE").unwrap_or_else(|_| "http://127.0.0.1:1200".into()),
    };
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--help" | "-h" => {
                println!("feedpush {} — RSSHub feed → 去重 → Telegram-ready 文字\n\n\
用法:\n  feedpush [--test] [--feed <id>] [--config <path>] [--base <url>]\n\n\
選項:\n  --test            忽略去重，印出目前前 8 則\n  --feed <id>       只跑指定 feed（feeds.json 的 id）\n  --config <path>   指定 feeds.json（預設 ~/work/rss-feeds/feeds.json）\n  --base <url>      指定 RSSHub 位址（預設 http://127.0.0.1:1200）\n  -h, --help        顯示這份說明", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--test" => args.test = true,
            "--feed" => {
                if i + 1 < argv.len() {
                    args.feed = Some(argv[i + 1].clone());
                    i += 1;
                }
            }
            "--config" => {
                if i + 1 < argv.len() {
                    args.config = PathBuf::from(&argv[i + 1]);
                    i += 1;
                }
            }
            "--base" => {
                if i + 1 < argv.len() {
                    args.base = argv[i + 1].clone();
                    i += 1;
                }
            }
            other => eprintln!("未知參數：{other}"),
        }
        i += 1;
    }
    args
}

fn fetch(url: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .user_agent("Mozilla/5.0 (compatible; feedpush/0.1)")
        .build();
    match agent.get(url).call() {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| format!("讀取失敗: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(format!("連線失敗: {e}")),
    }
}

/// 解析 RSS 2.0（<item>）與 Atom（<entry>）最小交集：標題 + 連結
fn parse_items(xml: &str) -> Result<Vec<Item>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("XML 解析失敗: {e}"))?;
    let mut out = Vec::new();

    for node in doc.descendants() {
        let is_rss = node.has_tag_name("item");
        let is_atom = node.has_tag_name("entry");
        if !is_rss && !is_atom {
            continue;
        }

        let title = node
            .children()
            .find(|c| c.has_tag_name("title"))
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
            .unwrap_or_default();

        let link = node
            .children()
            .find(|c| c.has_tag_name("link"))
            .map(|c| {
                // Atom: <link href="..."/>；RSS: <link>...</link>
                c.attribute("href")
                    .map(|s| s.to_string())
                    .or_else(|| c.text().map(|t| t.trim().to_string()))
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        if title.is_empty() || link.is_empty() {
            continue;
        }
        out.push(Item {
            id: link.clone(), // 用連結當唯一鍵（標題可能重複）
            title,
            link,
        });
    }
    Ok(out)
}

fn matches_keywords(item: &Item, keywords: &[String]) -> bool {
    if keywords.is_empty() {
        return true;
    }
    let t = item.title.to_lowercase();
    keywords.iter().any(|k| t.contains(&k.to_lowercase()))
}

fn state_path(config: &Path, feed_id: &str) -> PathBuf {
    let dir = config.parent().unwrap_or(Path::new(".")).join("state");
    let _ = fs::create_dir_all(&dir);
    dir.join(format!("{feed_id}.json"))
}

fn load_state(path: &Path) -> State {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<State>(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, mut state: State) -> io::Result<()> {
    if state.seen.len() > 400 {
        let keep = state.seen.len() - 400;
        state.seen.drain(0..keep);
    }
    let json = serde_json::to_string_pretty(&state)?;
    fs::write(path, json)
}

fn build_message(feed: &Feed, items: &[Item]) -> String {
    let mut text = format!("📰 {}（{} 則）", feed.name, items.len());
    if text.len() > MAX_CHARS {
        return text;
    }
    for it in items {
        let block = format!("\n• {}\n  {}", it.title, it.link);
        if text.len() + block.len() > MAX_CHARS - 60 {
            text.push_str("\n…（其餘請開 RSSHub 看）");
            break;
        }
        text.push_str(&block);
    }
    text
}

fn main() {
    let args = parse_args();

    let raw = match fs::read_to_string(&args.config) {
        Ok(s) => s,
        Err(e) => {
            println!("⚠️ 讀不到 feeds.json（{}）：{e}", args.config.display());
            return;
        }
    };
    let cfg: Config = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            println!("⚠️ feeds.json 格式錯誤：{e}");
            return;
        }
    };

    let feeds: Vec<Feed> = cfg
        .feeds
        .into_iter()
        .filter(|f| f.enabled)
        .filter(|f| args.feed.as_ref().map(|id| id == &f.id).unwrap_or(true))
        .collect();

    if feeds.is_empty() {
        eprintln!("找不到符合的 feed");
        std::process::exit(1);
    }

    let mut blocks: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for feed in &feeds {
        let url = format!(
            "{}/{}",
            args.base.trim_end_matches('/'),
            feed.route.trim_start_matches('/')
        );
        let body = match fetch(&url) {
            Ok(b) => b,
            Err(e) => {
                failures.push(format!("{}：{e}", feed.name));
                continue;
            }
        };
        let mut items = match parse_items(&body) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}：{e}", feed.name));
                continue;
            }
        };
        if !feed.keywords.is_empty() {
            items.retain(|i| matches_keywords(i, &feed.keywords));
        }

        let path = state_path(&args.config, &feed.id);
        if args.test {
            let fresh: Vec<Item> = items.iter().take(MAX_ITEMS).cloned().collect();
            if !fresh.is_empty() {
                blocks.push(build_message(feed, &fresh));
            }
            continue;
        }

        let state = load_state(&path);
        let seen: HashSet<&String> = state.seen.iter().collect();
        let fresh: Vec<Item> = items
            .iter()
            .filter(|i| !seen.contains(&i.id))
            .take(MAX_ITEMS)
            .cloned()
            .collect();

        // 重要：把「這次抓到的全部」都記成已見，只推最新的 N 則。
        // 只記有推的那幾則 → 舊文章會在下一輪變成「新」的，backlog 一則一則漏出來。
        let mut merged = state;
        let known: HashSet<String> = merged.seen.iter().cloned().collect();
        for it in &items {
            if !known.contains(&it.id) {
                merged.seen.push(it.id.clone());
            }
        }
        merged.updated = "written".into();
        if let Err(e) = save_state(&path, merged) {
            failures.push(format!("{}：寫入 state 失敗 {e}", feed.name));
        }

        if !fresh.is_empty() {
            blocks.push(build_message(feed, &fresh));
        }
    }

    let mut stdout = io::stdout();
    if blocks.is_empty() {
        if failures.len() == feeds.len() && !feeds.is_empty() {
            // 全部取數失敗才出聲（單一 feed 掛掉只記 stderr）
            let _ = writeln!(stdout, "⚠️ RSSHub 取數失敗：{}", failures.join("；"));
        }
    } else {
        let _ = writeln!(stdout, "{}", blocks.join("\n\n"));
    }
    // 全部失敗時上面已經印過警示，不要再重複印一份到 stderr
    if !blocks.is_empty() {
        for f in &failures {
            eprintln!("{f}");
        }
    }
}
