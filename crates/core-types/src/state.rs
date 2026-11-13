use crate::{CoreError, CoreResult};

/// ストリーミングエンジンの動作状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineState {
    /// 初期状態・デバイス待機中
    Idle,
    /// キャプチャとGPU合成・ローカル/MSEプレビューのみ稼働中
    Previewing,
    /// プレビューに加えて RTMPS 配信稼働中
    Streaming,
    /// ネットワーク切断後の自動再接続待機中
    Reconnecting,
    /// 回復不能エラー停止
    Error,
}

/// 不正な状態遷移を防止する状態機械
#[derive(Debug, Clone)]
pub struct StateMachine {
    current: PipelineState,
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            current: PipelineState::Idle,
        }
    }

    #[inline]
    pub fn current(&self) -> PipelineState {
        self.current
    }

    pub fn transition_to(&mut self, next: PipelineState) -> CoreResult<PipelineState> {
        use PipelineState::*;
        let valid = match (self.current, next) {
            (Idle, Previewing) => true,
            (Previewing, Streaming) => true,
            (Previewing, Idle) => true,
            (Streaming, Previewing) => true,
            (Streaming, Reconnecting) => true,
            (Reconnecting, Streaming) => true,
            (Reconnecting, Previewing) => true,
            (Reconnecting, Idle) => true,
            (_, Error) => true,
            (Error, Idle) => true,
            (Error, Previewing) => true,
            (Error, Streaming) => true,
            (a, b) if a == b => true,
            _ => false,
        };

        if valid {
            self.current = next;
            Ok(self.current)
        } else {
            Err(CoreError::InvalidStateTransition {
                from: self.current,
                to: next,
            })
        }
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}
