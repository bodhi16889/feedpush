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
/// 去重清單上限：超過就丟掉最舊的（cron 一天跑一次，400 筆約可回溯兩週）
const SEEN_CAP: usize = 400;

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
        Ok(resp) => resp.into_string().map_err(|e| format!("讀取失敗: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(format!("連線失敗: {e}")),
    }
}

/// 把 base 與 route 接起來，兩邊多餘的斜線都吃掉
fn feed_url(base: &str, route: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        route.trim_start_matches('/')
    )
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

fn save_state(path: &Path, state: State) -> io::Result<()> {
    let json = serde_json::to_string_pretty(&state)?;
    fs::write(path, json)
}

/// 合併「這次抓到的全部」進 state，並回傳這次要推的新項目。
///
/// 關鍵：**整批都標記已見，只推最新 limit 則**。若只標記推出去的那幾則，
/// 沒推到的舊文章下一輪又會變成「新」的，backlog 會一天漏一則慢慢洗版。
fn merge_state(mut state: State, fetched: &[Item], limit: usize) -> (State, Vec<Item>) {
    let mut known: HashSet<String> = state.seen.iter().cloned().collect();
    let mut fresh: Vec<Item> = Vec::new();

    for it in fetched {
        if known.insert(it.id.clone()) {
            state.seen.push(it.id.clone());
            if fresh.len() < limit {
                fresh.push(it.clone());
            }
        }
    }

    if state.seen.len() > SEEN_CAP {
        let drop_n = state.seen.len() - SEEN_CAP;
        state.seen.drain(0..drop_n);
    }
    state.updated = "written".into();
    (state, fresh)
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
        let url = feed_url(&args.base, &feed.route);
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

        let (merged, fresh) = merge_state(load_state(&path), &items, MAX_ITEMS);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(name: &str) -> Feed {
        Feed {
            id: "t".into(),
            name: name.into(),
            route: "t".into(),
            enabled: true,
            keywords: vec![],
        }
    }

    fn item(id: &str, title: &str) -> Item {
        Item {
            id: id.into(),
            title: title.into(),
            link: id.into(),
        }
    }

    fn items(n: usize) -> Vec<Item> {
        (0..n)
            .map(|i| item(&format!("https://example.com/{i}"), &format!("標題 {i}")))
            .collect()
    }

    // ---------- XML 解析 ----------

    #[test]
    fn parses_rss_items_in_order() {
        let xml = r#"<?xml version="1.0"?>
        <rss><channel><title>頻道</title>
          <item><title>第一則</title><link>https://a/1</link></item>
          <item><title>第二則</title><link>https://a/2</link></item>
        </channel></rss>"#;
        let got = parse_items(xml).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].title, "第一則");
        assert_eq!(got[0].id, "https://a/1");
        assert_eq!(got[1].link, "https://a/2");
    }

    #[test]
    fn parses_atom_entries_with_href() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <entry><title>Atom 一</title><link href="https://b/1"/></entry>
          <entry><title>Atom 二</title><link href="https://b/2"/></entry>
        </feed>"#;
        let got = parse_items(xml).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "https://b/1");
        assert_eq!(got[1].title, "Atom 二");
    }

    #[test]
    fn skips_item_without_link() {
        let xml = r#"<rss><channel>
          <item><title>沒有連結</title></item>
          <item><title>有連結</title><link>https://a/1</link></item>
        </channel></rss>"#;
        let got = parse_items(xml).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "有連結");
    }

    #[test]
    fn skips_item_with_empty_title() {
        let xml = r#"<rss><channel>
          <item><title>   </title><link>https://a/1</link></item>
          <item><title>正常</title><link>https://a/2</link></item>
        </channel></rss>"#;
        let got = parse_items(xml).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "https://a/2");
    }

    #[test]
    fn trims_whitespace_in_title_and_link() {
        let xml = r#"<rss><channel>
          <item><title>
            前後有空白
          </title><link>  https://a/1  </link></item>
        </channel></rss>"#;
        let got = parse_items(xml).unwrap();
        assert_eq!(got[0].title, "前後有空白");
        assert_eq!(got[0].link, "https://a/1");
    }

    #[test]
    fn ignores_non_item_nodes() {
        let xml = r#"<rss><channel><item><title>x</title><link>https://a/1</link></item>
          <title>頻道標題不該被當成項目</title></channel></rss>"#;
        assert_eq!(parse_items(xml).unwrap().len(), 1);
    }

    #[test]
    fn invalid_xml_is_an_error() {
        assert!(parse_items("<rss><channel>").is_err());
    }

    // ---------- 關鍵字過濾 ----------

    #[test]
    fn empty_keywords_match_everything() {
        assert!(matches_keywords(&item("u", "任何標題"), &[]));
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let kw = vec!["ai".to_string()];
        assert!(matches_keywords(&item("u", "OpenAI ships GPT"), &kw));
        assert!(matches_keywords(&item("u", "生成式 AI 新創"), &kw));
    }

    #[test]
    fn chinese_keyword_matches() {
        let kw = vec!["半導體".to_string(), "台股".to_string()];
        assert!(matches_keywords(&item("u", "半導體景氣回溫"), &kw));
        assert!(matches_keywords(&item("u", "台股收紅"), &kw));
    }

    #[test]
    fn keyword_miss_is_filtered_out() {
        let kw = vec!["半導體".to_string()];
        assert!(!matches_keywords(&item("u", "永續獎得獎名單公布"), &kw));
    }

    // ---------- 訊息組裝 ----------

    #[test]
    fn message_has_header_and_bullets() {
        let msg = build_message(&feed("測試台"), &items(2));
        assert!(msg.starts_with("📰 測試台（2 則）"), "實際：{msg}");
        assert_eq!(msg.matches("\n• ").count(), 2);
        assert!(msg.contains("https://example.com/0"));
    }

    #[test]
    fn short_message_is_not_truncated() {
        let msg = build_message(&feed("x"), &items(2));
        assert!(!msg.contains("其餘"));
    }

    #[test]
    fn long_message_truncates_under_telegram_limit() {
        let long: Vec<Item> = (0..200)
            .map(|i| {
                item(
                    &format!("https://example.com/{i}"),
                    &format!("很長的標題 {i} {}", "字".repeat(60)),
                )
            })
            .collect();
        let msg = build_message(&feed("x"), &long);
        assert!(msg.contains("…（其餘請開 RSSHub 看）"), "應該截斷");
        assert!(msg.len() <= MAX_CHARS, "實際長度 {}", msg.len());
    }

    // ---------- 去重（最會出錯的地方） ----------

    #[test]
    fn first_run_pushes_at_most_limit() {
        let (state, fresh) = merge_state(State::default(), &items(20), 8);
        assert_eq!(fresh.len(), 8, "只推最新 8 則");
        assert_eq!(state.seen.len(), 20, "整批都要標記已見");
    }

    #[test]
    fn second_run_is_silent() {
        let batch = items(20);
        let (state, _) = merge_state(State::default(), &batch, 8);
        let (_, fresh) = merge_state(state, &batch, 8);
        assert!(fresh.is_empty(), "同一批跑第二次不該再推");
    }

    #[test]
    fn no_backlog_leak_when_feed_grows() {
        // 第一輪 20 則（只推 8），第二輪原 20 則 + 1 則新的 → 只推新的那一則
        let batch = items(20);
        let (state, _) = merge_state(State::default(), &batch, 8);

        let mut next = batch.clone();
        next.push(item("https://example.com/new", "全新的那一則"));
        let (state2, fresh) = merge_state(state, &next, 8);

        assert_eq!(fresh.len(), 1, "只該有 1 則新的，實際 {}", fresh.len());
        assert_eq!(fresh[0].id, "https://example.com/new");
        assert_eq!(state2.seen.len(), 21);
    }

    #[test]
    fn duplicate_ids_inside_one_batch_count_once() {
        let dup = vec![item("https://a/1", "甲"), item("https://a/1", "甲（重複）")];
        let (state, fresh) = merge_state(State::default(), &dup, 8);
        assert_eq!(fresh.len(), 1);
        assert_eq!(state.seen, vec!["https://a/1".to_string()]);
    }

    #[test]
    fn seen_list_is_capped_and_keeps_newest() {
        let batch = items(500);
        let (state, _) = merge_state(State::default(), &batch, 8);
        assert_eq!(state.seen.len(), SEEN_CAP);
        assert_eq!(state.seen.last().unwrap(), "https://example.com/499");
        assert_eq!(
            state.seen.first().unwrap(),
            "https://example.com/100",
            "應丟掉最舊的 100 筆"
        );
    }

    #[test]
    fn limit_zero_pushes_nothing_but_still_records() {
        let (state, fresh) = merge_state(State::default(), &items(3), 0);
        assert!(fresh.is_empty());
        assert_eq!(state.seen.len(), 3);
    }

    #[test]
    fn merge_marks_updated_field() {
        let (state, _) = merge_state(State::default(), &items(1), 8);
        assert_eq!(state.updated, "written");
    }

    #[test]
    fn empty_fetch_keeps_state_untouched() {
        let before = State {
            seen: vec!["https://old/1".into()],
            updated: "old".into(),
        };
        let (after, fresh) = merge_state(before, &[], 8);
        assert!(fresh.is_empty());
        assert_eq!(after.seen, vec!["https://old/1".to_string()]);
    }

    // ---------- state 檔 ----------

    #[test]
    fn state_json_roundtrip() {
        let s = State {
            seen: vec!["https://a/1".into(), "https://a/2".into()],
            updated: "2026-11-25".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen, s.seen);
        assert_eq!(back.updated, s.updated);
    }

    #[test]
    fn state_tolerates_missing_fields() {
        let s: State = serde_json::from_str("{}").unwrap();
        assert!(s.seen.is_empty());
        assert_eq!(s.updated, "");
    }

    #[test]
    fn load_state_falls_back_to_default_on_broken_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("feedpush-broken-{}.json", std::process::id()));
        fs::write(&path, "{ this is not json").unwrap();
        assert!(load_state(&path).seen.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn state_path_sits_next_to_config() {
        let p = state_path(Path::new("/tmp/cfg/feeds.json"), "hn");
        assert_eq!(p, PathBuf::from("/tmp/cfg/state/hn.json"));
    }

    // ---------- 設定檔 ----------

    #[test]
    fn config_defaults_enabled_true_and_empty_keywords() {
        let cfg: Config =
            serde_json::from_str(r#"{"feeds":[{"id":"a","name":"A","route":"a/b"}]}"#).unwrap();
        assert!(cfg.feeds[0].enabled);
        assert!(cfg.feeds[0].keywords.is_empty());
    }

    #[test]
    fn config_reads_keywords_and_disabled_flag() {
        let cfg: Config = serde_json::from_str(
            r#"{"feeds":[{"id":"a","name":"A","route":"a/b","enabled":false,
                 "keywords":["AI","半導體"]}]}"#,
        )
        .unwrap();
        assert!(!cfg.feeds[0].enabled);
        assert_eq!(cfg.feeds[0].keywords, vec!["AI", "半導體"]);
    }

    // ---------- URL ----------

    #[test]
    fn feed_url_joins_without_double_slash() {
        assert_eq!(
            feed_url("http://127.0.0.1:1200/", "/hackernews/best"),
            "http://127.0.0.1:1200/hackernews/best"
        );
        assert_eq!(
            feed_url("http://127.0.0.1:1200", "cna/business"),
            "http://127.0.0.1:1200/cna/business"
        );
        assert_eq!(
            feed_url("https://rsshub.example.com", "/cna/business/"),
            "https://rsshub.example.com/cna/business/"
        );
    }
}
