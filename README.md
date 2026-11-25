# feedpush

把自架 [RSSHub](https://github.com/DIYgod/RSSHub) 的 feed 抓下來、去重、輸出成可直接丟給 Telegram 的文字。單一執行檔，不需要 Python。

給 cron / systemd timer 用的「watchdog」型工具：**沒有新項目就完全不輸出**，所以掛在每小時甚至每 10 分鐘都不會吵。

## 為什麼不是 Python

同一個功能有一份 Python 版（`push.py`）。這份 Rust 版的價值只有兩個：

| | Rust | Python |
|---|---|---|
| 執行檔 | 1.3 MB 單一 binary | 需要 Python 3.9+ |
| 啟動到結束（同一台 Mac mini、本機 RSSHub、5 次中位數） | **13.7 ms** | 58.2 ms |
| 輸出 | 逐行 byte 相同（實測 `diff` 為空） | — |

## 安裝（預編好的執行檔）

到 [Releases](https://github.com/bodhi16889/feedpush/releases/latest) 下載對應平台，旁邊附 `.sha256`：

```bash
# Apple Silicon
curl -LO https://github.com/bodhi16889/feedpush/releases/latest/download/feedpush-aarch64-apple-darwin
chmod +x feedpush-aarch64-apple-darwin && mv feedpush-aarch64-apple-darwin ~/bin/feedpush

# Intel Mac
curl -LO https://github.com/bodhi16889/feedpush/releases/latest/download/feedpush-x86_64-apple-darwin

# Linux x86_64（static-pie，可直接丟 scratch image）
curl -LO https://github.com/bodhi16889/feedpush/releases/latest/download/feedpush-x86_64-unknown-linux-musl
```

## 用法

```bash
feedpush                       # 正常（cron 用）：只推上次跑完後的新項目
feedpush --test                # 忽略去重，印出目前前 8 則
feedpush --feed hackernews     # 只跑指定 feed
feedpush --config ./feeds.json # 指定設定檔
feedpush --base http://127.0.0.1:1200
feedpush --help
```

輸出長這樣（`--test`）：

```
📰 Hacker News 熱門（8 則）
• Matrix Multiplication is Θ(n^2.25) [pdf]
  https://github.com/openai/math/...
• Tidal bans AI-generated music
  https://www.theverge.com/...
```

## 設定檔

```json
{
  "feeds": [
    { "id": "hackernews", "name": "Hacker News 熱門", "route": "hackernews/best", "enabled": true },
    { "id": "cna-business", "name": "中央社 產經", "route": "cna/business", "enabled": false,
      "keywords": ["AI", "半導體", "台股", "營收", "財報", "央行", "匯率", "關稅"] }
  ]
}
```

- `route` 對應 RSSHub 的路徑；加之前先確認 `curl -o /dev/null -w "%{http_code}" http://127.0.0.1:1200/<route>` 回 200。
- `keywords` 留空＝全推；有填＝只推標題含關鍵字（適合新聞稿很多、訊號很雜的站）。
- 去重狀態存在設定檔同層的 `state/<id>.json`（只留最近 400 筆）。

## 接到 Telegram（Hermes cron 範例）

```sh
#!/bin/sh
# ~/.hermes/scripts/rss_feeds.sh
exec "$HOME/bin/feedpush" "$@"
```

```
cronjob_manage action=create name="HN 日報" no_agent=true \
  script=rss_feeds.sh schedule="10 8 * * 1-5"
```

`no_agent` 模式會把 stdout 原文送出；空白就不發訊息。

## 行為細節（都是踩過的坑）

1. **去重要一次標記整批**：只記「這次推出去的」，剩下的舊文章會在下一輪變成新的，一則一則漏出來。這版把抓到的全部寫進 `state`，只推最新 N 則。
2. **失敗不當 crash**：單一 feed 掛掉只寫 stderr；全部掛掉才輸出一行告警，且 `exit 0`（不要讓 cron 當成程式壞掉）。
3. **長度**：輸出自動截在 3800 字元內（Telegram 上限 4096）。
4. 支援 RSS 2.0 的 `<item>` 與 Atom 的 `<entry>`。

## 建置

```bash
cargo build --release      # target/release/feedpush
cargo test                 # 目前 0 個測試：邏輯以 --test 對照 RSSHub 實跑驗證
```

依賴：`ureq`（同步 HTTP + rustls）、`roxmltree`（唯讀 XML）、`serde_json`。沒有 tokio、沒有 clap。

## 授權

MIT
