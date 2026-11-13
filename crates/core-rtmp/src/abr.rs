/// AIMD（Additive Increase / Multiplicative Decrease）動的ビットレート制御（ABR）設定
#[derive(Debug, Clone)]
pub struct AbrConfig {
    pub initial_bitrate_bps: u32,
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    /// 帯域削減を発動する送信滞留時間の閾値（既定: 400ms）
    pub decrease_dwell_threshold_ms: u64,
    /// 帯域削減倍率（既定: 0.75）
    pub decrease_factor: f64,
    /// 連続した帯域削減を防ぐクールダウン時間（既定: 1000ms）
    pub decrease_cooldown_ms: u64,
    /// 非IDRフレーム緊急スキップを発動する滞留時間の閾値（既定: 1000ms）
    pub emergency_drop_dwell_ms: u64,
    /// 帯域回復判定に用いる低滞留閾値（既定: 100ms）
    pub increase_dwell_threshold_ms: u64,
    /// 低滞留状態が継続して初めて回復ステップを行う期間（既定: 5000ms）
    pub increase_hold_ms: u64,
    /// 1ステップあたりの回復増分（既定: +250kbps）
    pub increase_step_bps: u32,
}

impl Default for AbrConfig {
    fn default() -> Self {
        Self {
            initial_bitrate_bps: 4_000_000,
            min_bitrate_bps: 400_000,
            max_bitrate_bps: 8_000_000,
            decrease_dwell_threshold_ms: 400,
            decrease_factor: 0.75,
            decrease_cooldown_ms: 1_000,
            emergency_drop_dwell_ms: 1_000,
            increase_dwell_threshold_ms: 100,
            increase_hold_ms: 5_000,
            increase_step_bps: 250_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbrDecision {
    pub current_bitrate_bps: u32,
    /// ビットレートが変化した場合のみ `Some(new_bps)`
    pub new_bitrate_bps: Option<u32>,
    /// 滞留が 1000ms を超えて非 IDR 映像フレームのスキップが必要か
    pub drop_non_idr: bool,
    /// 緊急スキップから復帰する際に IDR キーフレーム要求を出すべきか
    pub request_keyframe: bool,
    pub dwell_ms: u64,
}

/// 送信キューの推定滞留時間 (`queued_bytes * 8 / current_bitrate`) を監視し、
/// AIMD アルゴリズムで映像ターゲットビットレートを制御するコントローラ
pub struct AimdAbrController {
    cfg: AbrConfig,
    current_bitrate_bps: u32,
    cooldown_remaining_ms: u64,
    healthy_streak_ms: u64,
    in_emergency_drop: bool,
}

impl AimdAbrController {
    pub fn new(cfg: AbrConfig) -> Self {
        let initial = cfg
            .initial_bitrate_bps
            .clamp(cfg.min_bitrate_bps, cfg.max_bitrate_bps);
        Self {
            cfg,
            current_bitrate_bps: initial,
            cooldown_remaining_ms: 0,
            healthy_streak_ms: 0,
            in_emergency_drop: false,
        }
    }

    pub fn current_bitrate_bps(&self) -> u32 {
        self.current_bitrate_bps
    }

    /// 推定滞留時間（ミリ秒）を算出する
    #[inline]
    pub fn estimate_dwell_ms(&self, queued_bytes: usize) -> u64 {
        let bps = self.current_bitrate_bps.max(1) as u128;
        ((queued_bytes as u128 * 8 * 1_000) / bps) as u64
    }

    /// 前回評価からの経過時間 `elapsed_ms`（通常 200ms 周期）と現在の未送信バイト数 `queued_bytes` を基に判定を行う
    pub fn evaluate(&mut self, queued_bytes: usize, elapsed_ms: u64) -> AbrDecision {
        let dwell_ms = self.estimate_dwell_ms(queued_bytes);
        self.cooldown_remaining_ms = self.cooldown_remaining_ms.saturating_sub(elapsed_ms);

        let prev_bitrate = self.current_bitrate_bps;
        let prev_emergency = self.in_emergency_drop;

        // 1. 緊急フレームスキップ判定（> 1000ms）
        self.in_emergency_drop = dwell_ms > self.cfg.emergency_drop_dwell_ms;
        let request_keyframe = prev_emergency && !self.in_emergency_drop;

        // 2. Multiplicative Decrease（> 400ms で 0.75 倍へ減衰）
        if dwell_ms > self.cfg.decrease_dwell_threshold_ms {
            self.healthy_streak_ms = 0;
            if self.cooldown_remaining_ms == 0 {
                let reduced = ((self.current_bitrate_bps as f64) * self.cfg.decrease_factor)
                    .round() as u32;
                self.current_bitrate_bps = reduced.max(self.cfg.min_bitrate_bps);
                self.cooldown_remaining_ms = self.cfg.decrease_cooldown_ms;
            }
        } else if dwell_ms < self.cfg.increase_dwell_threshold_ms {
            // 3. Additive Increase（< 100ms が 5 秒継続で +250kbps 段階回復）
            self.healthy_streak_ms = self.healthy_streak_ms.saturating_add(elapsed_ms);
            if self.healthy_streak_ms >= self.cfg.increase_hold_ms {
                self.healthy_streak_ms = 0;
                if self.current_bitrate_bps < self.cfg.max_bitrate_bps {
                    self.current_bitrate_bps = (self
                        .current_bitrate_bps
                        .saturating_add(self.cfg.increase_step_bps))
                    .min(self.cfg.max_bitrate_bps);
                }
            }
        } else {
            // 中間帯域 (100ms..=400ms): 安定継続カウントをリセットして現状維持
            self.healthy_streak_ms = 0;
        }

        let new_bitrate_bps = if self.current_bitrate_bps != prev_bitrate {
            Some(self.current_bitrate_bps)
        } else {
            None
        };

        AbrDecision {
            current_bitrate_bps: self.current_bitrate_bps,
            new_bitrate_bps,
            drop_non_idr: self.in_emergency_drop,
            request_keyframe,
            dwell_ms,
        }
    }
}
