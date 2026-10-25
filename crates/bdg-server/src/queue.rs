//! ジョブキュー。Discordのレート制限とディスク/メモリの負荷を抑えるため、
//! 実行は常に1つずつ。同じ内容の待ちジョブはまとめ、連続pushは1回にする。

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// 待ちジョブの上限。超えたら受け付けない(503)。
pub const MAX_PENDING: usize = 100;
/// 履歴として覚えるジョブ数。
pub const MAX_HISTORY: usize = 200;
/// デバウンスで延ばせる最大の待ち。pushが続いても、最初の受信からこれ以上は待たせない。
pub const MAX_COALESCE_WAIT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// `repo` が `None` なら全対象。
    Backup { repo: Option<String>, force: bool },
    Prune { keep: Option<usize>, dry_run: bool },
    /// 全対象のバックアップ+世代整理(定期実行)。
    Scheduled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: u64,
    /// `manual` / `webhook` / `schedule`
    pub source: String,
    pub action: Action,
    pub status: Status,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// 実行結果の要約(リポジトリごとの1行)。秘密は含めない。
    pub messages: Vec<String>,
}

struct Pending {
    id: u64,
    not_before: Instant,
    first_enqueued: Instant,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    pending: Vec<Pending>,
    /// 新しい順。待ち・実行中・完了のすべてを含む。
    jobs: VecDeque<Job>,
}

pub struct Queue {
    inner: Mutex<Inner>,
    notify: Notify,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EnqueueError {
    Full,
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Queue {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner::default()), notify: Notify::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// ジョブを積む。同じ内容の待ちジョブがあれば新規には作らず、
    /// その実行時刻を `delay` 後へ延ばす(ただし最初の受信から
    /// [`MAX_COALESCE_WAIT`] まで)。戻り値は `(ジョブID, まとめたか)`。
    pub fn enqueue(&self, source: &str, action: Action, delay: Duration) -> Result<(u64, bool), EnqueueError> {
        let now = Instant::now();
        let mut q = self.lock();
        let existing = q
            .pending
            .iter()
            .position(|p| q.jobs.iter().any(|j| j.id == p.id && j.action == action && j.status == Status::Queued));
        if let Some(i) = existing {
            let p = &mut q.pending[i];
            let cap = p.first_enqueued + MAX_COALESCE_WAIT;
            p.not_before = (now + delay).min(cap);
            let id = p.id;
            drop(q);
            self.notify.notify_one();
            return Ok((id, true));
        }
        if q.pending.len() >= MAX_PENDING {
            return Err(EnqueueError::Full);
        }
        q.next_id += 1;
        let id = q.next_id;
        q.pending.push(Pending { id, not_before: now + delay, first_enqueued: now });
        q.jobs.push_front(Job {
            id,
            source: source.to_string(),
            action,
            status: Status::Queued,
            queued_at: now_unix(),
            started_at: None,
            finished_at: None,
            messages: Vec::new(),
        });
        while q.jobs.len() > MAX_HISTORY {
            // 古い完了済みから捨てる(待ち・実行中は残す)。
            if let Some(pos) = q.jobs.iter().rposition(|j| !matches!(j.status, Status::Queued | Status::Running)) {
                q.jobs.remove(pos);
            } else {
                break;
            }
        }
        drop(q);
        self.notify.notify_one();
        Ok((id, false))
    }

    pub fn get(&self, id: u64) -> Option<Job> {
        self.lock().jobs.iter().find(|j| j.id == id).cloned()
    }

    pub fn list(&self, limit: usize) -> Vec<Job> {
        self.lock().jobs.iter().take(limit).cloned().collect()
    }

    pub fn counts(&self) -> (usize, usize) {
        let q = self.lock();
        let queued = q.jobs.iter().filter(|j| j.status == Status::Queued).count();
        let running = q.jobs.iter().filter(|j| j.status == Status::Running).count();
        (queued, running)
    }

    /// 実行時刻を過ぎた待ちジョブのうち一番早いものを取り出して実行中にする。
    /// 無ければ次の実行時刻までの待ち時間(待ちも無ければ `None`)。
    fn take_due(&self) -> Result<(u64, Action), Option<Duration>> {
        let now = Instant::now();
        let mut q = self.lock();
        let due = q.pending.iter().enumerate().filter(|(_, p)| p.not_before <= now).min_by_key(|(_, p)| p.not_before).map(|(i, _)| i);
        match due {
            Some(i) => {
                let p = q.pending.remove(i);
                let action = match q.jobs.iter_mut().find(|j| j.id == p.id) {
                    Some(j) => {
                        j.status = Status::Running;
                        j.started_at = Some(now_unix());
                        j.action.clone()
                    }
                    None => return Err(Some(Duration::ZERO)),
                };
                Ok((p.id, action))
            }
            None => Err(q.pending.iter().map(|p| p.not_before.saturating_duration_since(now)).min()),
        }
    }

    fn finish(&self, id: u64, ok: bool, messages: Vec<String>) {
        let mut q = self.lock();
        if let Some(j) = q.jobs.iter_mut().find(|j| j.id == id) {
            j.status = if ok { Status::Succeeded } else { Status::Failed };
            j.finished_at = Some(now_unix());
            j.messages = messages;
        }
    }

    /// ワーカー本体。`run` は `(成功したか, 要約)` を返す。
    pub async fn work<F, Fut>(&self, run: F)
    where
        F: Fn(Action) -> Fut,
        Fut: std::future::Future<Output = (bool, Vec<String>)>,
    {
        loop {
            match self.take_due() {
                Ok((id, action)) => {
                    let (ok, msgs) = run(action).await;
                    self.finish(id, ok, msgs);
                }
                Err(wait) => {
                    // 通知を取りこぼさないよう、待つ前に登録する。
                    let notified = self.notify.notified();
                    match wait {
                        Some(d) => {
                            let _ = tokio::time::timeout(d, notified).await;
                        }
                        None => notified.await,
                    }
                }
            }
        }
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn backup(repo: &str) -> Action {
        Action::Backup { repo: Some(repo.into()), force: false }
    }

    #[test]
    fn identical_pending_jobs_coalesce_and_delay_is_capped() {
        let q = Queue::new();
        let (a, c1) = q.enqueue("webhook", backup("o/r"), Duration::from_secs(60)).unwrap();
        let (b, c2) = q.enqueue("webhook", backup("o/r"), Duration::from_secs(60)).unwrap();
        let (c, c3) = q.enqueue("webhook", backup("o/other"), Duration::from_secs(60)).unwrap();
        assert_eq!(a, b);
        assert!(!c1 && c2 && !c3);
        assert_ne!(a, c);
        assert_eq!(q.counts().0, 2);
        // 延長は最初の受信から上限まで。
        let long = Duration::from_secs(100_000);
        q.enqueue("webhook", backup("o/r"), long).unwrap();
        let inner = q.lock();
        let p = inner.pending.iter().find(|p| p.id == a).unwrap();
        assert!(p.not_before <= p.first_enqueued + MAX_COALESCE_WAIT);
    }

    #[test]
    fn queue_is_bounded() {
        let q = Queue::new();
        for i in 0..MAX_PENDING {
            q.enqueue("manual", backup(&format!("o/r{i}")), Duration::from_secs(60)).unwrap();
        }
        assert_eq!(q.enqueue("manual", backup("o/overflow"), Duration::ZERO), Err(EnqueueError::Full));
        // 既にある内容へのまとめは、満杯でも受け付ける。
        assert!(q.enqueue("manual", backup("o/r0"), Duration::ZERO).unwrap().1);
    }

    #[tokio::test]
    async fn worker_runs_jobs_one_at_a_time_in_due_order() {
        let q = Arc::new(Queue::new());
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let running = Arc::new(Mutex::new(0u32));
        let max_running = Arc::new(Mutex::new(0u32));
        let (q2, log2, run2, max2) = (q.clone(), log.clone(), running.clone(), max_running.clone());
        let worker = tokio::spawn(async move {
            q2.work(|a| {
                let (log, running, max) = (log2.clone(), run2.clone(), max2.clone());
                async move {
                    {
                        let mut r = running.lock().unwrap();
                        *r += 1;
                        let mut m = max.lock().unwrap();
                        *m = (*m).max(*r);
                    }
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    if let Action::Backup { repo: Some(r), .. } = a {
                        log.lock().unwrap().push(r.clone());
                        *running.lock().unwrap() -= 1;
                        return (r != "o/bad", vec![format!("done {r}")]);
                    }
                    *running.lock().unwrap() -= 1;
                    (true, vec![])
                }
            })
            .await
        });
        let (late, _) = q.enqueue("manual", backup("o/late"), Duration::from_millis(150)).unwrap();
        let (bad, _) = q.enqueue("manual", backup("o/bad"), Duration::ZERO).unwrap();
        let (first, _) = q.enqueue("manual", backup("o/first"), Duration::ZERO).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*max_running.lock().unwrap(), 1, "jobs must never overlap");
        assert_eq!(log.lock().unwrap().last().unwrap(), "o/late");
        assert_eq!(q.get(late).unwrap().status, Status::Succeeded);
        assert_eq!(q.get(bad).unwrap().status, Status::Failed);
        assert_eq!(q.get(first).unwrap().messages, vec!["done o/first".to_string()]);
        // 新しい順に並ぶ。
        assert_eq!(q.list(10)[0].id, first);
        worker.abort();
    }
}
