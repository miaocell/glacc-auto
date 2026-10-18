// 业务主域客户端（任务列表 / 钱包 / mobileGLTaskPush 直推），逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Glacc\GlaccGameClient.cs（对应抓包脚本 _reverse/scripts/push_test.py）。
//
// 请求头来自抓包 §四；任务配置每日可变，masterTaskId / taskId / stageSum 一律从
// mobileGLTaskList 动态读取，绝不硬编码。服务端每日下发多个主任务（点广告、每日任务、
// 下载&浏览、限时补充、愉悦赚…），默认全部纳入领取。
// 全部请求经 session::Session 执行：网络失败按预算重试，JWT 失效自动换新后重试。
//
// 与旧版唯一的有意差异（已记录）：push 结果未知时的旁路对账，旧版经 GetTaskStagesAsync
// 走完整 Session（自带重试预算与 JWT 续期），Rust 侧 Session::run 持有 &mut cred，
// 无法在 run 内部再嵌套一次 run（借用冲突）。故对账探测改为单次只读 GET：
// 探测本身只用于"服务端是否已记账"的判定，失败即返回 None、交回外层正常重试，
// 最终结果与旧版一致；差别仅在服务端持续故障时少发几次探测请求（更保守）。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::glacc::credentials::Credentials;
use crate::glacc::result::{GlaccCallResult, GlaccFailReason, GlaccHttpResponse};
use crate::glacc::session::Session;
use crate::glacc::tls::{self, TlsError, TlsRequestPayload, TlsResponse};
use crate::glacc::{constants, device, sign, store};

/// 广告任务阶段进度（来自 mobileGLTaskList，任务配置每日可变，禁止硬编码），
/// 对应旧版 GlaccTaskStage。master_task_id 随阶段携带：push 的签名与请求体都用它。
/// spark 三项是服务端在领取次数之外单列的一份奖励，现有协议未提供领取入口，只做只读呈现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStage {
    pub master_task_id: i32,
    pub task_id: i32,
    pub name: String,
    pub stage_current: i32,
    pub stage_sum: i32,
    /// 该阶段全天的奖励分值（服务端"total"字段）；÷ SCORE_PER_MINUTE 得分钟数
    pub total_score: i32,
    pub spark_enabled: bool,
    pub spark_reward_minutes: i32,
    pub spark_claimed: bool,
}

/// mobileGLTaskPush 响应（code=0 时 add_score 为本次发放分值），对应旧版 GlaccPushResult。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushResult {
    pub code: i32,
    pub add_score: i64,
    pub task_name: String,
}

/// HTTP 传输层（测试可替换成脚本化 Mock，绝不触网）。
pub type Transport = Arc<dyn Fn(&TlsRequestPayload) -> GlaccHttpResponse + Send + Sync>;

/// 默认传输：走 tls-client 原生库（OkHttp/Android 指纹）。
/// 旧版 SendAsync 的异常映射：GlaccTlsUnavailableException → ClientError，其余 → 未取得响应。
pub fn native_transport() -> Transport {
    Arc::new(|payload: &TlsRequestPayload| match tls::send(payload) {
        Ok(resp) => GlaccHttpResponse::new(Some(resp), "", GlaccFailReason::None),
        Err(TlsError::Unavailable(e)) => {
            store::log(&format!("TLS 指纹库不可用：{e}"));
            GlaccHttpResponse::from_client_fault(e)
        }
        Err(e) => GlaccHttpResponse::no_response(e.to_string()),
    })
}

/// 时间源（签名时间戳；测试可固定）。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn native_clock() -> Clock {
    Arc::new(Credentials::now_seconds)
}

/// 业务主域客户端：持有 Session 的独占借用（同步模型，调用方须在后台线程执行）。
pub struct GameClient<'s, 'a> {
    session: &'s mut Session<'a>,
    transport: Transport,
    clock: Clock,
}

/// 一次请求的固定部分（URL / 头部 / TLS 指纹），body 每次尝试现生成。
struct Prepared {
    url: String,
    headers: Vec<(String, String)>,
    identifier: String,
}

impl Prepared {
    fn new(cred: &Credentials, url: String, with_json: bool) -> Self {
        let device = cred.device();
        let peer = cred.peer_id();
        // 头部顺序有意义：header_order 直接由它派生，会决定发出的头部顺序与 TLS 指纹。
        let mut headers: Vec<(String, String)> = vec![
            ("appid".to_string(), constants::APP_ID.to_string()),
            ("package_name".to_string(), constants::PACKAGE_NAME.to_string()),
            ("package-name".to_string(), constants::PACKAGE_NAME.to_string()),
            ("x-client-type".to_string(), "glacc_android".to_string()),
            ("userid".to_string(), cred.sub.clone()),
            ("user-id".to_string(), cred.sub.clone()),
            ("app-version".to_string(), constants::APP_VERSION.to_string()),
            ("peerid".to_string(), peer.clone()),
            ("install-channel".to_string(), "official".to_string()),
            ("x-channel-id".to_string(), "official".to_string()),
            ("x-device-id".to_string(), peer.clone()),
            ("x-guid".to_string(), peer.clone()),
            (
                "authorization".to_string(),
                format!("Bearer {}", cred.access_token),
            ),
            ("client_version".to_string(), constants::APP_VERSION.to_string()),
            ("user-agent".to_string(), device::ua_game(&device)),
        ];
        if with_json {
            headers.push(("content-type".to_string(), "application/json".to_string()));
        }
        Self {
            url,
            headers,
            identifier: device::identifier_for(&device),
        }
    }

    fn payload(&self, method: &str, body: Option<String>) -> TlsRequestPayload {
        TlsRequestPayload {
            tls_client_identifier: self.identifier.clone(),
            request_method: method.to_string(),
            request_url: self.url.clone(),
            request_body: body,
            header_order: Some(self.headers.iter().map(|(k, _)| k.clone()).collect()),
            headers: self.headers.clone(),
            timeout_seconds: 15,
            follow_redirects: true,
            without_cookie_jar: true,
            is_byte_request: false,
            is_byte_response: false,
            catch_panics: true,
        }
    }
}

/// mobileGLTaskPush 请求体（ext_data 不参与签名，保持空对象）。
#[derive(Debug, Serialize)]
struct PushBody {
    #[serde(rename = "masterTaskId")]
    master_task_id: i32,
    #[serde(rename = "taskId")]
    task_id: i32,
    #[serde(rename = "timestamp")]
    timestamp: i64,
    #[serde(rename = "extData")]
    ext_data: BTreeMap<String, String>,
    #[serde(rename = "sign")]
    sign: String,
}

/// 业务接口抽象：ClaimRunner 依赖它做单元测试（Mock 不跑真实网络）。
pub trait GameApi {
    /// 任务列表。require_non_empty = true 时把空列表视为失败（对账与领取前都要求非空）。
    fn get_task_stages(
        &mut self,
        master_task_id: Option<i32>,
        require_non_empty: bool,
        on_retry: Option<&mut dyn FnMut(i32, Duration)>,
    ) -> GlaccCallResult<Vec<TaskStage>>;

    /// 钱包分值（score，与"可用时长"按 80:1 分钟换算）。
    fn get_wallet_score(
        &mut self,
        on_retry: Option<&mut dyn FnMut(i32, Duration)>,
    ) -> GlaccCallResult<i64>;

    /// 真签名直推一次 mobileGLTaskPush。local_current = 本地已确认的该阶段完成次数，
    /// 结果未知时用于旁路对账（服务端已记账则不再重发）。
    fn push_task(
        &mut self,
        master_task_id: i32,
        task_id: i32,
        local_current: i32,
        on_retry: Option<&mut dyn FnMut(i32, Duration)>,
    ) -> GlaccCallResult<PushResult>;
}

impl<'s, 'a> GameClient<'s, 'a> {
    pub fn new(session: &'s mut Session<'a>) -> Self {
        Self {
            session,
            transport: native_transport(),
            clock: native_clock(),
        }
    }

    /// 替换传输层（测试用）。
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// 替换时间源（测试用）。
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

/// 任务列表的有效性判定（拒绝空列表）。
type StageAccept<'x> = Option<&'x dyn Fn(&Vec<TaskStage>) -> bool>;

impl<'s, 'a> GameApi for GameClient<'s, 'a> {
    fn get_task_stages(
        &mut self,
        master_task_id: Option<i32>,
        require_non_empty: bool,
        on_retry: Option<&mut dyn FnMut(i32, Duration)>,
    ) -> GlaccCallResult<Vec<TaskStage>> {
        if !self.session.cred().has_token() {
            return GlaccCallResult::fail(GlaccFailReason::NotLoggedIn, "");
        }
        let url = format!(
            "{}/xlppc.gacs/api/gxsdn/act/advert/mobileGLTaskList?appid={}",
            constants::GAME_BASE,
            constants::APP_ID
        );
        let transport = self.transport.clone();
        let send = move |cred: &Credentials| {
            transport(&Prepared::new(cred, url.clone(), false).payload("GET", None))
        };

        let accept_fn = |v: &Vec<TaskStage>| !v.is_empty();
        let accept: StageAccept = if require_non_empty {
            Some(&accept_fn)
        } else {
            None
        };
        self.session.run(
            send,
            |resp| parse_stages(resp, master_task_id),
            accept,
            on_retry,
            "拉取任务进度",
            None,
        )
    }

    fn get_wallet_score(
        &mut self,
        on_retry: Option<&mut dyn FnMut(i32, Duration)>,
    ) -> GlaccCallResult<i64> {
        if !self.session.cred().has_token() {
            return GlaccCallResult::fail(GlaccFailReason::NotLoggedIn, "");
        }
        let url = format!(
            "{}/xlppc.gacs/api/gxsdn/gold/get_user_wallet",
            constants::GAME_BASE
        );
        let transport = self.transport.clone();
        let send = move |cred: &Credentials| {
            transport(&Prepared::new(cred, url.clone(), false).payload("GET", None))
        };
        self.session
            .run(send, parse_wallet, None, on_retry, "查询余额", None)
    }

    fn push_task(
        &mut self,
        master_task_id: i32,
        task_id: i32,
        local_current: i32,
        on_retry: Option<&mut dyn FnMut(i32, Duration)>,
    ) -> GlaccCallResult<PushResult> {
        if !self.session.cred().has_token() {
            return GlaccCallResult::fail(GlaccFailReason::NotLoggedIn, "");
        }
        let push_url = format!(
            "{}/xlppc.gacs/api/gxsdn/act/advert/mobileGLTaskPush",
            constants::GAME_BASE
        );
        let stages_url = format!(
            "{}/xlppc.gacs/api/gxsdn/act/advert/mobileGLTaskList?appid={}",
            constants::GAME_BASE,
            constants::APP_ID
        );
        let transport = self.transport.clone();
        let clock = self.clock.clone();
        // 每次尝试现取时间戳（与旧版 send 闭包内计算 ts 一致），头部随当前凭据重建（JWT 换新后须带新 token）。
        let send = move |cred: &Credentials| {
            let ts = clock();
            let body = PushBody {
                master_task_id,
                task_id,
                timestamp: ts,
                ext_data: BTreeMap::new(),
                sign: sign::push_sign(master_task_id, task_id, ts),
            };
            // 旧版 source-gen 上下文 WriteIndented=true，保持同样的输出形态。
            let json = serde_json::to_string_pretty(&body).unwrap_or_default();
            transport(&Prepared::new(cred, push_url.clone(), true).payload("POST", Some(json)))
        };

        // 旁路对账：结果未知（5xx / 无业务判定）时先确认服务端是否已记账。
        let probe_transport = self.transport.clone();
        let probe_url = stages_url;
        let mut hook = move |cred: &Credentials| {
            recover_committed(
                &probe_transport,
                &Prepared::new(cred, probe_url.clone(), false),
                master_task_id,
                task_id,
                local_current,
            )
        };
        self.session.run(
            send,
            parse_push,
            None,
            on_retry,
            &format!("推送任务 {task_id}"),
            Some(&mut hook),
        )
    }
}

/// push 结果未知时的旁路确认：重查服务端进度，若该任务阶段完成数已超过本地确认值，
/// 说明先前那次推送已在服务端落账（返回 add_score=0 表示"已发放、分值未知"），据此停止重发。
/// 查询失败或进度未推进时返回 None，交回执行器按预算重试。
fn recover_committed(
    transport: &Transport,
    prepared: &Prepared,
    master_task_id: i32,
    task_id: i32,
    local_current: i32,
) -> Option<PushResult> {
    let http = transport(&prepared.payload("GET", None));
    let resp = http.response?;
    if resp.status >= 500 {
        return None;
    }
    let stages = parse_stages(&resp, Some(master_task_id))?;
    let server = stages.iter().find(|s| s.task_id == task_id)?;
    if server.stage_current <= local_current {
        return None;
    }
    store::log(&format!(
        "推送任务 {task_id} 结果未知，但服务端已记账（{local_current} → {}），视为已发放，不再重发",
        server.stage_current
    ));
    Some(PushResult {
        code: 0,
        add_score: 0,
        task_name: server.name.clone(),
    })
}

// ── 响应解析（返回 None 即视为请求失败，由执行器计入重试预算）──

/// 展开任务列表为阶段序列：纳入服务端下发的全部主任务；master_task_id 非空时只取该主任务
/// （供单任务旁路对账）。阶段名带上主任务名（去掉"移动端-"前缀）：多主任务下"阶段一"等名称
/// 会重复，需要可区分。
///
/// 与旧版一致：JSON 可解析但缺 data/list 时返回空列表（成功），不是失败；
/// stageSum<=0 是今日不可领的预留槽位，直接跳过（纳入会导致空推）。
fn parse_stages(resp: &TlsResponse, master_task_id: Option<i32>) -> Option<Vec<TaskStage>> {
    let root: serde_json::Value = serde_json::from_str(&resp.body).ok()?;
    let Some(list) = root
        .get("data")
        .and_then(|d| d.get("list"))
        .and_then(|l| l.as_array())
    else {
        return Some(Vec::new());
    };

    let mut stages = Vec::new();
    for master in list {
        let Some(mid) = master.get("masterTaskId").and_then(read_i32) else {
            continue;
        };
        if let Some(want) = master_task_id {
            if mid != want {
                continue;
            }
        }
        let Some(details) = master
            .get("taskDetailList")
            .and_then(|d| d.as_array())
        else {
            continue;
        };
        let master_name = trim_master_name(&read_text(master, "taskName"));
        let mut i = 1;
        for t in details {
            let Some(tid) = t.get("taskId").and_then(read_i32) else {
                continue;
            };
            let task_name = read_text(t, "taskName");
            let name = if !task_name.is_empty() {
                if master_name.is_empty() {
                    task_name
                } else {
                    format!("{master_name}·{task_name}")
                }
            } else {
                format!("{master_name}·阶段{i}")
            };
            let sum = read_int(t, "stageSum");
            let cur = read_int(t, "stageCurrent");
            // 阶段全天奖励分值（total）：÷ SCORE_PER_MINUTE 得分钟数，供首页预估可领用
            let stage_total = read_int(t, "total");
            // 火花奖励：服务端在领取次数之外单列，本程序只读呈现，不提供领取
            let spark_on = read_bool(t, "sparkEnabled");
            let spark_min = read_int(t, "sparkRewardMinutes");
            let spark_got = read_bool(t, "sparkClaimed");
            if sum > 0 {
                stages.push(TaskStage {
                    master_task_id: mid,
                    task_id: tid,
                    name,
                    stage_current: cur,
                    stage_sum: sum,
                    total_score: stage_total,
                    spark_enabled: spark_on,
                    spark_reward_minutes: spark_min,
                    spark_claimed: spark_got,
                });
            }
            i += 1;
        }
    }
    Some(stages)
}

/// 主任务名统一去掉"移动端-"前缀，使阶段名简洁且不与官方分组标题重复。
fn trim_master_name(master_name: &str) -> String {
    master_name
        .strip_prefix("移动端-")
        .unwrap_or(master_name)
        .to_string()
}

/// 字符串字段：非字符串（含 null/数字）一律按空串处理，与旧版 ReadText 一致。
fn read_text(el: &serde_json::Value, name: &str) -> String {
    el.get(name)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// JSON 整数 → i32；非整数（字符串/小数/超范围）一律失败。
fn read_i32(v: &serde_json::Value) -> Option<i32> {
    v.as_i64()
        .filter(|n| *n >= i32::MIN as i64 && *n <= i32::MAX as i64)
        .map(|n| n as i32)
}

/// 数值字段兜底读取：取不到或非数一律 0（与旧版 ReadInt 同语义），另接受数字字符串，
/// 防止服务端把进度/分值偶发下发成字符串时把核心进度读丢。身份字段仍走 read_i32 严格整数。
fn read_int(el: &serde_json::Value, name: &str) -> i32 {
    let Some(v) = el.get(name) else {
        return 0;
    };
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
        .filter(|n| *n >= i32::MIN as i64 && *n <= i32::MAX as i64)
        .map(|n| n as i32)
        .unwrap_or(0)
}

fn read_bool(el: &serde_json::Value, name: &str) -> bool {
    el.get(name).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// 钱包：data.score。缺字段/非整数即解析失败（旧版 GetProperty/GetInt64 抛异常）。
fn parse_wallet(resp: &TlsResponse) -> Option<i64> {
    let root: serde_json::Value = serde_json::from_str(&resp.body).ok()?;
    root.get("data")?.get("score")?.as_i64()
}

/// 解析 push 响应。响应里没有数值 code = 未取得业务判定（网关错误页、空对象等），
/// 返回 None 交给执行器按"结果未知"走旁路对账，而不是误判成 code=-1 的"服务端拒绝"而跳过阶段。
fn parse_push(resp: &TlsResponse) -> Option<PushResult> {
    let root: serde_json::Value = serde_json::from_str(&resp.body).ok()?;
    let code = root.get("code").and_then(read_i32)?;
    let mut add_score = 0i64;
    let mut task_name = String::new();
    if code == 0 {
        if let Some(data) = root.get("data") {
            if let Some(score) = data.get("addScore") {
                if score.is_number() {
                    // 与旧版 GetInt64 一致：非整数分值视为响应无法解析
                    add_score = score.as_i64()?;
                }
            }
            if let Some(name) = data.get("taskName").and_then(|v| v.as_str()) {
                task_name = name.to_string();
            }
        }
    }
    Some(PushResult {
        code,
        add_score,
        task_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glacc::session::{default_sleeper, Sleeper};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const SALT: &str = "0123456789abcdef0123456789abcdef";

    fn cred() -> Credentials {
        Credentials {
            phone: "13800138000".to_string(),
            install_salt: SALT.to_string(),
            sub: "sub-1".to_string(),
            access_token: "token".to_string(),
            ..Credentials::default()
        }
    }

    fn ok_json(body: &str) -> GlaccHttpResponse {
        GlaccHttpResponse::new(
            Some(TlsResponse {
                status: 200,
                body: body.to_string(),
            }),
            "",
            GlaccFailReason::None,
        )
    }

    fn http_status(status: i32) -> GlaccHttpResponse {
        GlaccHttpResponse::new(
            Some(TlsResponse {
                status,
                body: String::new(),
            }),
            "",
            if status >= 500 {
                GlaccFailReason::ServerError
            } else {
                GlaccFailReason::None
            },
        )
    }

    /// 脚本化传输层：按顺序返回预设响应，并记录每次请求的载荷。
    struct Scripted {
        script: Mutex<VecDeque<GlaccHttpResponse>>,
        seen: Mutex<Vec<TlsRequestPayload>>,
    }

    impl Scripted {
        fn new(script: Vec<GlaccHttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into()),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn transport(self: &Arc<Self>) -> Transport {
            let me = self.clone();
            Arc::new(move |p: &TlsRequestPayload| {
                me.seen.lock().unwrap().push(p.clone());
                me.script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| GlaccHttpResponse::no_response("脚本已耗尽"))
            })
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|p| (p.request_method.clone(), p.request_url.clone()))
                .collect()
        }
    }

    fn recording_sleeper() -> (Sleeper, Arc<Mutex<Vec<u64>>>) {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let h = hits.clone();
        let s: Sleeper = Arc::new(move |d: Duration| h.lock().unwrap().push(d.as_secs()));
        (s, hits)
    }

    fn fixed_clock(ts: i64) -> Clock {
        Arc::new(move || ts)
    }

    /// 列表解析与旧版逐项一致：多主任务、前缀裁剪、空名回退"阶段N"、
    /// stageSum<=0 跳过、缺 masterTaskId/taskId 跳过（且不计入阶段序号）。
    #[test]
    fn parse_stages_matches_legacy() {
        let body = r#"{
          "code": 0,
          "data": { "list": [
            { "masterTaskId": 52, "taskName": "移动端-点广告得时长", "taskDetailList": [
                {"taskId": 178, "taskName": "阶段一", "stageSum": 3, "stageCurrent": 1, "total": 2400,
                 "sparkEnabled": true, "sparkRewardMinutes": 5, "sparkClaimed": false},
                {"taskName": "无 id", "stageSum": 9},
                {"taskId": 179, "taskName": "", "stageSum": 2, "stageCurrent": 2, "total": 1600},
                {"taskId": 180, "taskName": "预留", "stageSum": 0, "stageCurrent": 0}
            ]},
            { "masterTaskId": 78, "taskName": "移动端-每日任务", "taskDetailList": [
                {"taskId": 201, "taskName": "浏览", "stageSum": 1, "stageCurrent": 0, "total": 800}
            ]},
            { "taskName": "缺主任务 id", "taskDetailList": [{"taskId": 300, "stageSum": 1}] }
          ]}
        }"#;
        let resp = TlsResponse {
            status: 200,
            body: body.to_string(),
        };
        let stages = parse_stages(&resp, None).expect("应解析成功");
        assert_eq!(
            stages,
            vec![
                TaskStage {
                    master_task_id: 52,
                    task_id: 178,
                    name: "点广告得时长·阶段一".to_string(),
                    stage_current: 1,
                    stage_sum: 3,
                    total_score: 2400,
                    spark_enabled: true,
                    spark_reward_minutes: 5,
                    spark_claimed: false,
                },
                TaskStage {
                    master_task_id: 52,
                    task_id: 179,
                    name: "点广告得时长·阶段2".to_string(),
                    stage_current: 2,
                    stage_sum: 2,
                    total_score: 1600,
                    spark_enabled: false,
                    spark_reward_minutes: 0,
                    spark_claimed: false,
                },
                TaskStage {
                    master_task_id: 78,
                    task_id: 201,
                    name: "每日任务·浏览".to_string(),
                    stage_current: 0,
                    stage_sum: 1,
                    total_score: 800,
                    spark_enabled: false,
                    spark_reward_minutes: 0,
                    spark_claimed: false,
                },
            ]
        );

        // 只取指定主任务（旁路对账用）
        let only = parse_stages(&resp, Some(78)).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].task_id, 201);
    }

    /// 缺 data/list → 空列表（成功），与旧版 ParseStages 返回 [] 一致；非法 JSON → 失败。
    #[test]
    fn parse_stages_missing_list_is_empty_success() {
        let empty = TlsResponse {
            status: 200,
            body: r#"{"code":0}"#.to_string(),
        };
        assert_eq!(parse_stages(&empty, None), Some(Vec::new()));

        let bad = TlsResponse {
            status: 200,
            body: "not json".to_string(),
        };
        assert_eq!(parse_stages(&bad, None), None);
    }

    /// 钱包与 push 响应解析：非整数 code / addScore 视为失败。
    #[test]
    fn parse_wallet_and_push() {
        let wallet = TlsResponse {
            status: 200,
            body: r#"{"data":{"score":1600}}"#.to_string(),
        };
        assert_eq!(parse_wallet(&wallet), Some(1600));
        let no_score = TlsResponse {
            status: 200,
            body: r#"{"data":{}}"#.to_string(),
        };
        assert_eq!(parse_wallet(&no_score), None);

        let push = TlsResponse {
            status: 200,
            body: r#"{"code":0,"data":{"addScore":80,"taskName":"阶段一"}}"#.to_string(),
        };
        assert_eq!(
            parse_push(&push),
            Some(PushResult {
                code: 0,
                add_score: 80,
                task_name: "阶段一".to_string()
            })
        );

        let refused = TlsResponse {
            status: 200,
            body: r#"{"code":-1702}"#.to_string(),
        };
        assert_eq!(
            parse_push(&refused),
            Some(PushResult {
                code: -1702,
                add_score: 0,
                task_name: String::new()
            })
        );

        // 无 code = 未取得业务判定（网关错误页/空对象），必须走旁路对账
        let unknown = TlsResponse {
            status: 200,
            body: r#"{"message":"gateway"}"#.to_string(),
        };
        assert_eq!(parse_push(&unknown), None);
        let string_code = TlsResponse {
            status: 200,
            body: r#"{"code":"0"}"#.to_string(),
        };
        assert_eq!(parse_push(&string_code), None);
    }

    /// 请求构造保真：头部顺序 + 签名时间戳 + 请求体字段。时间戳固定为已知抓包样本，
    /// 签名须等于 sign::push_sign 的样本值。
    #[test]
    fn push_request_shape_matches_legacy() {
        let mock = Scripted::new(vec![ok_json(r#"{"code":0,"data":{"addScore":80}}"#)]);
        let mut c = cred();
        let mut session = Session::new(&mut c, || 0).with_sleeper(default_sleeper());
        let mut game = GameClient::new(&mut session)
            .with_transport(mock.transport())
            .with_clock(fixed_clock(1786116529));
        let r = game.push_task(78, 178, 0, None);
        assert!(r.ok(), "detail={}", r.detail);

        let seen = mock.seen.lock().unwrap();
        let p = &seen[0];
        assert_eq!(p.request_method, "POST");
        assert!(p.request_url.ends_with("/mobileGLTaskPush"));
        assert_eq!(p.timeout_seconds, 15);
        assert!(p.follow_redirects && p.without_cookie_jar && p.catch_panics);
        assert!(!p.is_byte_request && !p.is_byte_response);

        // 头部顺序与旧版 Dictionary 插入序逐项一致
        let keys: Vec<&str> = p.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "appid",
                "package_name",
                "package-name",
                "x-client-type",
                "userid",
                "user-id",
                "app-version",
                "peerid",
                "install-channel",
                "x-channel-id",
                "x-device-id",
                "x-guid",
                "authorization",
                "client_version",
                "user-agent",
                "content-type",
            ]
        );
        let order: Vec<&str> = p
            .header_order
            .as_ref()
            .unwrap()
            .iter()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(order, keys);
        assert_eq!(
            p.headers
                .iter()
                .find(|(k, _)| k == "authorization")
                .unwrap()
                .1,
            "Bearer token"
        );

        // 请求体：字段名与签名
        let body = p.request_body.as_ref().expect("push 必须有 body");
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 5);
        assert_eq!(v["masterTaskId"], 78);
        assert_eq!(v["taskId"], 178);
        assert_eq!(v["timestamp"], 1786116529i64);
        assert_eq!(v["extData"], serde_json::json!({}));
        assert_eq!(v["sign"], "7e63fa2e335360557eeb0b0507c3691d");
    }

    /// 5xx → 旁路对账发现服务端已记账 → 按成功返回，不重发、不退避。
    #[test]
    fn push_5xx_recovers_without_resend() {
        let mock = Scripted::new(vec![
            http_status(502),
            ok_json(
                r#"{"code":0,"data":{"list":[
                    {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                        {"taskId":178,"taskName":"阶段一","stageSum":3,"stageCurrent":1}
                    ]}
                ]}}"#,
            ),
        ]);
        let (sleeper, hits) = recording_sleeper();
        let mut c = cred();
        let mut session = Session::new(&mut c, || 3).with_sleeper(sleeper);
        let mut game = GameClient::new(&mut session).with_transport(mock.transport());
        let r = game.push_task(52, 178, 0, None);

        assert!(r.ok(), "旁路确认成功应视为成功");
        let v = r.value.unwrap();
        assert_eq!(v.code, 0);
        assert_eq!(v.add_score, 0, "已发放但分值未知");
        assert_eq!(v.task_name, "点广告得时长·阶段一");
        assert!(hits.lock().unwrap().is_empty(), "不应进入退避等待");
        let calls = mock.calls();
        assert_eq!(calls.len(), 2, "一次 push + 一次对账探测");
        assert_eq!(calls[1].0, "GET");
    }

    /// 5xx 且对账未推进 → 按预算重试：retry_count=2 → 3 次 push + 3 次探测，退避 2s/4s。
    #[test]
    fn push_5xx_retries_per_budget_when_unrecovered() {
        let unchanged = r#"{"code":0,"data":{"list":[
            {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                {"taskId":178,"taskName":"阶段一","stageSum":3,"stageCurrent":0}
            ]}
        ]}}"#;
        let mock = Scripted::new(vec![
            http_status(503),
            ok_json(unchanged),
            http_status(503),
            ok_json(unchanged),
            http_status(503),
            ok_json(unchanged),
        ]);
        let (sleeper, hits) = recording_sleeper();
        let mut c = cred();
        let mut session = Session::new(&mut c, || 2).with_sleeper(sleeper);
        let mut game = GameClient::new(&mut session).with_transport(mock.transport());
        let r = game.push_task(52, 178, 0, None);

        assert!(!r.ok());
        assert_eq!(r.reason, GlaccFailReason::ServerError);
        assert_eq!(mock.seen.lock().unwrap().len(), 6);
        assert_eq!(mock.calls().iter().filter(|(m, _)| m == "POST").count(), 3);
        assert_eq!(*hits.lock().unwrap(), vec![2, 4], "3 次尝试对应 2 次退避");
    }

    /// 客户端故障（指纹库缺失）不重试；未登录短路且不触网。
    #[test]
    fn client_fault_and_not_logged_in() {
        let mock = Scripted::new(vec![GlaccHttpResponse::from_client_fault("缺 dll")]);
        let (sleeper, hits) = recording_sleeper();
        let mut c = cred();
        let mut session = Session::new(&mut c, || 5).with_sleeper(sleeper);
        let mut game = GameClient::new(&mut session).with_transport(mock.transport());
        let r = game.get_task_stages(None, true, None);
        assert_eq!(r.reason, GlaccFailReason::ClientError);
        assert_eq!(mock.seen.lock().unwrap().len(), 1, "客户端故障不应重试");
        assert!(hits.lock().unwrap().is_empty());

        let mut anonymous = Credentials::default();
        let mut session2 = Session::new(&mut anonymous, || 0);
        let mut game2 = GameClient::new(&mut session2);
        let r2 = game2.get_wallet_score(None);
        assert_eq!(r2.reason, GlaccFailReason::NotLoggedIn);
        assert!(r2.value.is_none());
    }

    /// 空列表在 require_non_empty 时判失败（与旧版 accept: Count>0 一致）；不要求时成功。
    #[test]
    fn empty_stage_list_acceptance() {
        let empty = r#"{"code":0,"data":{"list":[]}}"#;
        let mock = Scripted::new(vec![ok_json(empty), ok_json(empty)]);
        let (sleeper, _) = recording_sleeper();
        let mut c = cred();
        let mut session = Session::new(&mut c, || 0).with_sleeper(sleeper);
        let mut game = GameClient::new(&mut session).with_transport(mock.transport());

        let strict = game.get_task_stages(Some(52), true, None);
        assert!(!strict.ok());
        assert_eq!(strict.reason, GlaccFailReason::BadResponse);
        assert_eq!(strict.detail, "响应内容不符合预期");

        let lax = game.get_task_stages(None, false, None);
        assert!(lax.ok());
        assert_eq!(lax.value, Some(Vec::new()));
    }

    /// 凭据新鲜度：请求头每次调用按当前凭据重建，不得缓存 token。
    /// 同一次 run 内的重试也走同一路径（Session::run 每次尝试把 &Credentials 传给 send），
    /// 该契约由类型保证；此处验证跨调用不残留旧值。
    #[test]
    fn headers_are_rebuilt_from_current_credentials() {
        let mock = Scripted::new(vec![
            ok_json(r#"{"data":{"score":80}}"#),
            ok_json(r#"{"data":{"score":160}}"#),
        ]);
        let mut c = cred();
        {
            let mut session = Session::new(&mut c, || 0).with_sleeper(recording_sleeper().0);
            let mut game = GameClient::new(&mut session).with_transport(mock.transport());
            assert!(game.get_wallet_score(None).ok());
        }
        // 模拟一次登录态换新（生产路径由 Session 内部刷新同一份凭据）
        c.access_token = "token-new".to_string();
        {
            let mut session = Session::new(&mut c, || 0).with_sleeper(recording_sleeper().0);
            let mut game = GameClient::new(&mut session).with_transport(mock.transport());
            assert!(game.get_wallet_score(None).ok());
        }

        let seen = mock.seen.lock().unwrap();
        let auth_of = |i: usize| {
            seen[i]
                .headers
                .iter()
                .find(|(k, _)| k == "authorization")
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(auth_of(0), "Bearer token");
        assert_eq!(auth_of(1), "Bearer token-new", "不得沿用旧 token");
    }

    /// 真实服务端 payload 回归（2026-10-18 抓包，五个主任务数值原样入库，仅重排格式）：
    /// 16 个阶段、合计 48 次、Σtotal = 96400 分 → 1205 分钟（20 时 5 分）。
    /// 守护点：解析层任何改动都不得让真实下发的核心进度与预估分值读丢。
    #[test]
    fn parse_stages_real_server_payload_2026_10_18() {
        let body = include_str!("fixtures/stages_real_20261018.json");
        let resp = TlsResponse {
            status: 200,
            body: body.to_string(),
        };
        let stages = parse_stages(&resp, None).expect("应解析成功");
        assert_eq!(stages.len(), 16, "stageSum>0 的阶段应全部纳入");
        let claimed: i32 = stages.iter().map(|s| s.stage_current).sum();
        let total: i32 = stages.iter().map(|s| s.stage_sum).sum();
        let score: i64 = stages.iter().map(|s| s.total_score.max(0) as i64).sum();
        assert_eq!((claimed, total, score), (48, 48, 96400));
        use crate::glacc::claim::score_to_minutes;
        assert_eq!(score_to_minutes(score).round_ties_even() as i32, 1205);
        // 抽查：主任务 53"初级"（每次观看+50 分钟，4 次，全天 16000 分）
        let first = stages.iter().find(|s| s.task_id == 128).expect("应含 taskId 128");
        assert_eq!(
            (first.name.as_str(), first.stage_sum, first.stage_current, first.total_score),
            ("每日任务·初级", 4, 4, 16000)
        );
    }

    /// 解析兜底：进度/分值偶发下发成数字字符串时不得读丢（read_int 宽容、身份字段仍严格）。
    #[test]
    fn parse_stages_tolerates_numeric_strings() {
        let body = r#"{
          "code": 0,
          "data": { "list": [
            { "masterTaskId": 52, "taskName": "移动端-点广告得时长", "taskDetailList": [
                {"taskId": 178, "taskName": "阶段一", "stageSum": "3", "stageCurrent": "1",
                 "total": "2400", "sparkRewardMinutes": "5"},
                {"taskId": 179, "taskName": "阶段二", "stageSum": 2, "stageCurrent": 0, "total": null}
            ]}
          ]}
        }"#;
        let resp = TlsResponse {
            status: 200,
            body: body.to_string(),
        };
        let stages = parse_stages(&resp, None).expect("应解析成功");
        assert_eq!(stages.len(), 2);
        assert_eq!(
            (stages[0].stage_sum, stages[0].stage_current, stages[0].total_score, stages[0].spark_reward_minutes),
            (3, 1, 2400, 5)
        );
        assert_eq!((stages[1].stage_sum, stages[1].total_score), (2, 0), "null 分值按 0 处理");
    }
}
