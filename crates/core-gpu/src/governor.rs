//! GPU シェーダーステージ制限およびフレームタイムに基づく動的オーバーレイ数制御

/// 同時動画オーバーレイ数・負荷制御ガバナ
#[derive(Debug, Clone)]
pub struct OverlayGovernor {
    hardware_cap: usize,
    min_overlays: usize,
    current_cap: usize,
    target_frame_time_us: u64,
    ewma_frame_time_us: f64,
    consecutive_over_budget: u32,
    consecutive_under_budget: u32,
    total_throttled_layers: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct GovernorStats {
    pub hardware_cap: usize,
    pub current_cap: usize,
    pub avg_frame_time_us: u64,
    pub total_throttled_layers: u64,
}

impl OverlayGovernor {
    /// `wgpu::Limits::max_sampled_textures_per_shader_stage` から初期上限を決定する
    /// （1 動画レイヤーあたり最大 2 サンプルドテクスチャ想定）
    pub fn from_limits(max_sampled_textures_per_stage: u32, target_frame_time_us: u64) -> Self {
        let hardware_cap = ((max_sampled_textures_per_stage / 2) as usize).clamp(1, 16);
        Self {
            hardware_cap,
            min_overlays: 1,
            current_cap: hardware_cap,
            target_frame_time_us: target_frame_time_us.max(1_000),
            ewma_frame_time_us: 0.0,
            consecutive_over_budget: 0,
            consecutive_under_budget: 0,
            total_throttled_layers: 0,
        }
    }

    pub fn hardware_cap(&self) -> usize {
        self.hardware_cap
    }

    pub fn current_cap(&self) -> usize {
        self.current_cap
    }

    /// 1 フレーム分の所要時間を記録し、必要なら同時動画オーバーレイ上限を引き下げ／回復する
    pub fn record_frame_time(&mut self, frame_time_us: u64) {
        let sample = frame_time_us as f64;
        if self.ewma_frame_time_us == 0.0 {
            self.ewma_frame_time_us = sample;
        } else {
            self.ewma_frame_time_us = self.ewma_frame_time_us * 0.8 + sample * 0.2;
        }

        let budget = self.target_frame_time_us as f64;
        if self.ewma_frame_time_us > budget * 0.90 {
            self.consecutive_over_budget += 1;
            self.consecutive_under_budget = 0;
            if self.consecutive_over_budget >= 3 && self.current_cap > self.min_overlays {
                self.current_cap -= 1;
                self.consecutive_over_budget = 0;
            }
        } else if self.ewma_frame_time_us < budget * 0.60 {
            self.consecutive_under_budget += 1;
            self.consecutive_over_budget = 0;
            if self.consecutive_under_budget >= 15 && self.current_cap < self.hardware_cap {
                self.current_cap += 1;
                self.consecutive_under_budget = 0;
            }
        } else {
            self.consecutive_over_budget = 0;
            self.consecutive_under_budget = 0;
        }
    }

    /// スロットリング発生数を記録する
    pub fn record_throttled(&mut self, dropped: usize) {
        self.total_throttled_layers += dropped as u64;
    }

    pub fn stats(&self) -> GovernorStats {
        GovernorStats {
            hardware_cap: self.hardware_cap,
            current_cap: self.current_cap,
            avg_frame_time_us: self.ewma_frame_time_us as u64,
            total_throttled_layers: self.total_throttled_layers,
        }
    }
}
