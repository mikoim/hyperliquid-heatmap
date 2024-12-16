// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use svg::node::element::Rectangle;
use svg::node::element::Text;
use svg::node::Text as TextNode;
use svg::Document;
use tauri::Emitter;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use url::Url;
use ordered_float::OrderedFloat;
use log::{info, error, warn, LevelFilter};
use svg::node::element::Group;
use svg::node::element::Line;

const HEATMAP_WIDTH: i32 = 1920;
const HEATMAP_HEIGHT: i32 = 1080;
const RIGHT_MARGIN: i32 = 300;  // 右側の余白
const ACTUAL_HEATMAP_WIDTH: i32 = HEATMAP_WIDTH - RIGHT_MARGIN;  // ヒートマップの実際の描画幅

#[derive(Debug, Deserialize)]
struct WsLevel {
    px: String,
    sz: String,
    n: i32,
}

#[derive(Debug, Deserialize)]
struct WsBook {
    coin: String,
    levels: Vec<Vec<WsLevel>>,
    time: i64,
}

#[derive(Debug, Deserialize)]
struct WsMessage {
    channel: String,
    data: WsBook,
}

#[derive(Debug, Serialize, Clone)]
struct HeatmapData {
    svg: String
}

struct OrderBookState {
    buy: BTreeMap<OrderedFloat<f64>, f64>,
    sell: BTreeMap<OrderedFloat<f64>, f64>,
    history: Vec<(i64, BTreeMap<OrderedFloat<f64>, f64>, BTreeMap<OrderedFloat<f64>, f64>)>,
}

impl OrderBookState {
    fn new() -> Self {
        Self {
            buy: BTreeMap::new(),
            sell: BTreeMap::new(),
            history: Vec::with_capacity(300), // 5分間のデータ（1秒あたり1フレーム）
        }
    }

    fn update_history(&mut self, timestamp: i64) {
        // 履歴を更新
        self.history.push((
            timestamp,
            self.buy.clone(),
            self.sell.clone(),
        ));

        // 5分（300秒）より古いデータを削除
        let five_minutes_ago = timestamp - 300_000; // ミリ秒位
        self.history.retain(|(ts, _, _)| *ts > five_minutes_ago);
    }
}

async fn start_websocket_connection(app_handle: tauri::AppHandle) {
    info!("Starting WebSocket connection...");
    let state = Arc::new(RwLock::new(OrderBookState::new()));
    let (tx, mut rx) = mpsc::channel(100);

    // WebSocket接続を開始（sig_figs = 5のみ）
    let tx = tx.clone();
    let url = Url::parse("wss://api.hyperliquid.xyz/ws").unwrap();
    
    let _handle = app_handle.clone();
    tokio::spawn(async move {
        let mut retry_count = 0;
        loop {
            info!("Connecting to WebSocket with sig_figs: 5 (attempt: {})", retry_count + 1);
            
            // 指数バックオフによる再試行待機
            if retry_count > 0 {
                let wait_time = std::cmp::min(1 << retry_count, 30); // 最大30秒
                tokio::time::sleep(tokio::time::Duration::from_secs(wait_time)).await;
            }

            match connect_async(url.clone()).await {
                Ok((mut ws_stream, _)) => {
                    info!("WebSocket connected");
                    let subscribe_msg = serde_json::json!({
                        "method": "subscribe",
                        "subscription": {
                            "type": "l2Book",
                            "coin": "@107",
                            "nSigFigs": 5
                        }
                    });

                    match ws_stream.send(tokio_tungstenite::tungstenite::Message::Text(subscribe_msg.to_string())).await {
                        Ok(_) => {
                            info!("Subscription message sent");
                            retry_count = 0; // 接続成功したらリトライカウントをリセット
                            
                            while let Some(msg) = ws_stream.next().await {
                                match msg {
                                    Ok(msg) => {
                                        match serde_json::from_str::<WsMessage>(&msg.to_string()) {
                                            Ok(parsed) => {
                                                if let Err(e) = tx.send(parsed).await {
                                                    error!("Failed to send message through channel: {:?}", e);
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                warn!("Failed to parse WebSocket message: {:?}", e);
                                                warn!("Message content: {}", msg.to_string());
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("WebSocket error: {:?}", e);
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to send subscription message: {:?}", e);
                            retry_count += 1;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to connect WebSocket: {:?}", e);
                    retry_count += 1;
                }
            }

            info!("WebSocket connection lost. Reconnecting...");
        }
    });

    // 受信したデータを処理
    let handle = app_handle.clone();
    tokio::spawn(async move {
        info!("Starting data processing loop...");
        while let Some(msg) = rx.recv().await {
            let mut state = state.write();
            
            // オーダーブックの更新
            state.buy.clear();  // 古いデータをクリア
            state.sell.clear();  // 古いデータをクリア
            
            for (i, levels) in msg.data.levels.iter().enumerate() {
                for level in levels {
                    match (level.px.parse::<f64>(), level.sz.parse::<f64>()) {
                        (Ok(price), Ok(size)) => {
                            info!("{}: {:?}", i, level);
                            if i == 0 {
                                state.buy.insert(OrderedFloat(price), size);
                            } else {
                                state.sell.insert(OrderedFloat(price), size);
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            error!("Failed to parse price or size: {:?}", e);
                            continue;
                        }
                    }
                }
            }

            // 履歴の更新
            state.update_history(msg.data.time);

            // ヒートマップの生成
            let heatmap = generate_heatmap(&state);
            
            // フロントエンドにデータを送信
            let payload = HeatmapData {
                svg: heatmap
            };
            
            if let Err(e) = handle.emit("orderbook-update", payload) {
                error!("Failed to emit event: {:?}", e);
            }
        }
    });
}

fn generate_heatmap(state: &OrderBookState) -> String {
    let mut document = Document::new()
        .set("width", "100%")
        .set("height", "100%")
        .set("viewBox", format!("0 0 {} {}", HEATMAP_WIDTH, HEATMAP_HEIGHT))
        .set("preserveAspectRatio", "xMidYMid meet");

    let max_size = state.history.iter()
        .flat_map(|(_, buy, sell)| buy.values().chain(sell.values()))
        .fold(0.0f64, |acc, &x| acc.max(x));

    if max_size <= 0.0 {
        warn!("No data available for heatmap");
        return document.to_string();
    }

    info!("Generating heatmap with max_size: {}", max_size);

    // 背景を追加（ヒートマップ部分のみ）
    let background = Rectangle::new()
        .set("x", 0)
        .set("y", 0)
        .set("width", ACTUAL_HEATMAP_WIDTH)  // 余白を除いた幅
        .set("height", "100%")
        .set("fill", "#000000");
    document = document.add(background);

    // 全価格範囲を計算
    let all_prices: Vec<f64> = state.history.iter()
        .flat_map(|(_, buy, sell)| {
            buy.keys().chain(sell.keys()).map(|k| k.into_inner())
        })
        .collect();

    let min_price = all_prices.iter().fold(f64::INFINITY, |a, &b| a.min(b));
    let max_price = all_prices.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
    let price_range = max_price - min_price;

    info!("Price range: {} to {} (width: {})", min_price, max_price, price_range);

    // 時系列データを描画（x座標を調整）
    if let Some(latest_ts) = state.history.last().map(|(ts, _, _)| ts) {
        let five_minutes_ago = latest_ts - 300_000;

        for (_i, (ts, buy, sell)) in state.history.iter().enumerate() {
            let time_x = ((ts - five_minutes_ago) as f64 / 300_000.0 * ACTUAL_HEATMAP_WIDTH as f64) as i32;
            let bar_width = (ACTUAL_HEATMAP_WIDTH as f64 / state.history.len() as f64).ceil() as i32 + 1;

            for (&price, &size) in sell.iter().rev() {
                let price_val = price.into_inner();
                let y = HEATMAP_HEIGHT as i32 - ((price_val - min_price) / price_range * HEATMAP_HEIGHT as f64) as i32;
                let alpha = (size / max_size).min(1.0);
                
                let rect = Rectangle::new()
                    .set("x", time_x)
                    .set("y", y)
                    .set("width", bar_width)
                    .set("height", 2)
                    .set("fill", format!("rgba(255, 0, 0, {})", alpha));

                document = document.add(rect);
            }

            for (&price, &size) in buy.iter() {
                let price_val = price.into_inner();
                let y = HEATMAP_HEIGHT as i32 - ((price_val - min_price) / price_range * HEATMAP_HEIGHT as f64) as i32;
                let alpha = (size / max_size).min(1.0);
                
                let rect = Rectangle::new()
                    .set("x", time_x)
                    .set("y", y)
                    .set("width", bar_width)
                    .set("height", 2)
                    .set("fill", format!("rgba(0, 255, 0, {})", alpha));

                document = document.add(rect);
            }

            // その時点でのMid価格を描画
            let best_buy = buy.keys().next_back().map(|x| x.into_inner()).unwrap_or(0.0);
            let best_sell = sell.keys().next().map(|x| x.into_inner()).unwrap_or(0.0);
            let mid = (best_buy + best_sell) / 2.0;

            // Mid Line (白)
            let mid_y = HEATMAP_HEIGHT as i32 - ((mid - min_price) / price_range * HEATMAP_HEIGHT as f64) as i32;
            let mid_line = Rectangle::new()
                .set("x", time_x)
                .set("y", mid_y)
                .set("width", bar_width)
                .set("height", 1)
                .set("fill", "rgba(255, 255, 255, 0.8)");
            document = document.add(mid_line);
        }
    }

    // 価格軸のグループを作成
    let mut price_axis_group = Group::new()
        .set("font-family", "Arial")
        .set("font-size", "14")
        .set("fill", "white");

    // 価格軸の目盛りを生成（10分割）
    let price_steps = 10;
    for i in 0..=price_steps {
        let price = min_price + (price_range * i as f64 / price_steps as f64);
        let y = (HEATMAP_HEIGHT as f64 * (1.0 - i as f64 / price_steps as f64)) as i32;
        
        // 価格ラベル
        let price_text = Text::new()
            .set("x", ACTUAL_HEATMAP_WIDTH + 20)  // ヒートマップの右側に配置
            .set("y", y + 5)
            .set("text-anchor", "start")
            .add(TextNode::new(format!("{:.3}", price)));
        
        // 目盛り線
        let tick_line = Line::new()
            .set("x1", ACTUAL_HEATMAP_WIDTH)  // ヒートマップの終端から開始
            .set("x2", ACTUAL_HEATMAP_WIDTH + 10)  // 少し右に伸ばす
            .set("y1", y)
            .set("y2", y)
            .set("stroke", "white")
            .set("stroke-width", 1);

        price_axis_group = price_axis_group.add(price_text).add(tick_line);
    }

    // // 最大注文量の表示位置を調整
    // if let (Some((&large_buy_price, &large_buy_size)), Some((&large_sell_price, &large_sell_size))) = (
    //     state.buy.iter().max_by_key(|(_, &size)| OrderedFloat(size)),
    //     state.sell.iter().max_by_key(|(_, &size)| OrderedFloat(size))
    // ) {
    //     // 最大Bid注文
    //     let max_bid_y = ((max_bid_price.into_inner() - min_price) / price_range * HEATMAP_HEIGHT as f64) as i32;
    //     let max_bid_text = Text::new()
    //         .set("x", ACTUAL_HEATMAP_WIDTH + 80)  // ヒートマップの右側に配置
    //         .set("y", max_bid_y)
    //         .set("text-anchor", "start")  // 左揃えに変更
    //         .set("fill", "red")
    //         .add(TextNode::new(format!("Large Bid: {:.3} ({:.2})", max_bid_price.into_inner(), max_bid_size)));

    //     // 最大Ask注文
    //     let max_ask_y = ((max_ask_price.into_inner() - min_price) / price_range * HEATMAP_HEIGHT as f64) as i32;
    //     let max_ask_text = Text::new()
    //         .set("x", ACTUAL_HEATMAP_WIDTH + 80)  // ヒートマップの右側に配置
    //         .set("y", max_ask_y)
    //         .set("text-anchor", "start")  // 左揃えに変更
    //         .set("fill", "green")
    //         .add(TextNode::new(format!("Large Ask: {:.3} ({:.2})", max_ask_price.into_inner(), max_ask_size)));

    //     price_axis_group = price_axis_group.add(max_bid_text).add(max_ask_text);
    // }

    // 価格軸グループをドキュメントに追加
    document = document.add(price_axis_group);

    document.to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // ログ設定を初期化
    env_logger::Builder::from_default_env()
        .filter_level(LevelFilter::Info)
        .init();

    info!("Starting application...");

    // Tokioランタイムをグローバルに設定
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime")
        .block_on(async {
            tauri::Builder::default()
                .plugin(tauri_plugin_opener::init())
                .setup(|app| {
                    info!("Setting up application...");
                    let handle = app.handle().clone();
                    
                    // WebSocket接続を開始
                    tokio::spawn(async move {
                        start_websocket_connection(handle).await;
                    });
                    
                    Ok(())
                })
                .run(tauri::generate_context!())
                .expect("error while running tauri application");
        });
}
