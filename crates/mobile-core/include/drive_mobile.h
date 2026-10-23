/*
 * drive-sync mobile core —— 稳定 C ABI（Android JNI / iOS 静态库共用）
 *
 * 版本：drive-sync v0.2.0（DM_ABI_VERSION = 1）
 * 包名：Rust crate `drive-mobile`（目录 crates/mobile-core），产物：
 *   - Android: libdrive_mobile.so   （crate-type cdylib）
 *   - iOS:     libdrive_mobile.a    （crate-type staticlib）
 *
 * 所有 `char *` 返回值都是 **Rust 分配、调用方拥有** 的 UTF-8 JSON 字符串，
 * 必须用 dm_string_free() 释放（不要用 free()/delete）。返回 NULL 仅表示内存分配失败；
 * 业务错误统一放在 JSON 的 {"ok":false,"error":"..."} 里。
 *
 * 线程模型：
 *   - 除 dm_stop / dm_send_clipboard_text（可在任意线程调用）外，dm_init/dm_load/
 *     dm_config_set_relay 应在启动同步前、同一线程调用一次。
 *   - dm_start 内部创建 tokio 运行时与后台线程，立即返回；连接/重连在后台进行。
 *   - dm_pair_start / dm_pair_join / dm_peers_json / dm_catalog_json 内部会阻塞
 *     （网络/磁盘），不要在主线程/UI 线程调用（Android 用后台线程，iOS 用 Task）。
 *   - 回调可能从任意后台线程触发，必须线程安全；回调参数只在回调期间有效，
 *     需要保存时请自行拷贝。
 *
 * 事件/JSON schema 见文件末尾注释与 docs/mobile-android.md。
 */

#ifndef DRIVE_MOBILE_H
#define DRIVE_MOBILE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DM_ABI_VERSION 1
#define DM_VERSION "0.2.0"

/* 不透明同步会话句柄（dm_start 返回，dm_stop 释放）。 */
typedef struct dm_handle dm_handle;

/*
 * 事件回调：event_json 为 NUL 结尾 UTF-8 JSON 对象，仅在回调期间有效。
 * 详细 type 列表见文件末尾。
 */
typedef void (*dm_event_cb)(const char *event_json, void *user_data);

/*
 * 远端剪切板文本回调：text_utf8 为 NUL 结尾 UTF-8 文本（仅 text/rich_text 类型会回调；
 * 图片等类型只发 clipboard_applied 事件）。from_device_id 可能为空串（尽力而为，
 * 权威来源见 clipboard_applied 事件）。参数仅在回调期间有效。
 */
typedef void (*dm_clipboard_cb)(const char *text_utf8,
                                const char *from_device_id,
                                void *user_data);

/* 库版本/ABI 版本（静态字符串，不需要释放）。 */
const char *dm_version(void);
int32_t dm_abi_version(void);

/*
 * 初始化设备身份（identity.json；不存在则创建）并设置进程级 data_dir。
 * 成功： {"ok":true,"created":true|false,"device_id":"dev-...","name":"设备名"}
 * 失败： {"ok":false,"error":"..."}
 * 返回的字符串需 dm_string_free()。
 */
char *dm_init(const char *data_dir, const char *device_name);

/*
 * 仅加载已存在的设备身份（不创建），并设置进程级 data_dir。
 * 成功： {"ok":true,"created":false,"device_id":"dev-...","name":"设备名"}
 * 失败： {"ok":false,"error":"..."}（如尚未初始化）
 */
char *dm_load(const char *data_dir);

/*
 * 保存 relay/token/设备名到 <data_dir>/config.json（data_dir 由 dm_init/dm_load 设置）。
 * 入参 json： {"relay_url":"ws://host:8787/ws","access_token":"...","device_name":"..."}
 *   - 字段均可选；字符串字段传空串表示清空，缺省表示保持原值。
 * 成功： {"ok":true,"relay_url":"...","device_name":"...","has_token":true}
 * 失败： {"ok":false,"error":"..."}
 */
char *dm_config_set_relay(const char *json);

/*
 * 启动后台同步运行时（连接 + 自动重连 + 剪切板/目录同步）。
 *   relay_url / token 可为 NULL 或 ""，此时使用 config.json 中保存的值。
 *   clipboard_cb / event_cb 可为 NULL；user_data 原样回传给回调。
 * 成功返回不透明句柄；失败返回 NULL，错误信息见 dm_last_error()。
 * 同一进程同时只允许一个活动会话；重复调用返回已有句柄（幂等）。
 */
dm_handle *dm_start(const char *relay_url,
                    const char *token,
                    dm_clipboard_cb clipboard_cb,
                    dm_event_cb event_cb,
                    void *user_data);

/* 停止后台运行时并释放句柄（最多等待约 3 秒；可重复调用/传 NULL）。 */
void dm_stop(dm_handle *handle);

/*
 * 发送本机剪切板文本（UTF-8）。0 = 已投递，<0 = 错误：
 *   -1 句柄无效/已停止，-2 文本非法或超限，-3 运行时未连接（稍后会自动重试/进离线队列）。
 */
int32_t dm_send_clipboard_text(dm_handle *handle, const char *text);

/*
 * 暂停（paused != 0）/恢复（paused == 0）剪切板同步（不影响连接与目录同步）。
 * 暂停期间：不再把远端文本写入系统剪贴板，dm_send_clipboard_text 返回 -3。
 * 0 = 成功，-1 = 句柄无效。
 */
int32_t dm_set_paused(dm_handle *handle, int32_t paused);

/*
 * 发起配对（A 端）。阻塞直到生成 8 位配对码（通常 <2s，最多约 30s），返回后配对继续在后台进行，
 * 结果通过事件回调 pair_code / pair_succeeded / pair_failed 通知。
 * 成功： {"ok":true,"code":"12345678","qr":"dsync-pair-v1:..."}
 * 失败： {"ok":false,"error":"..."}
 * 注意：配对需要独占中继连接，内部会临时暂停同步会话，配对结束后自动恢复。
 */
char *dm_pair_start(dm_handle *handle);

/*
 * 加入配对（B 端）：code_or_qr 为 8 位数字配对码，或 dsync-pair-v1:... 扫码串。
 * 阻塞直到配对完成（最多约 130 秒）；SAS 在原型阶段自动确认并通过事件 pair_succeeded 展示。
 * 成功： {"ok":true,"peer_id":"dev-...","name":"...","sas":"123456"}
 * 失败： {"ok":false,"error":"..."}
 */
char *dm_pair_join(dm_handle *handle, const char *code_or_qr);

/*
 * 已配对设备列表 + 在线状态。
 * 成功： {"ok":true,"self":{"device_id":"dev-...","name":"..."},"peers":[
 *          {"device_id":"dev-...","name":"...","label":"...","relay_url":"ws://...",
 *           "paired_at_ms":1700000000000,"online":true,"session":true}]}
 * 失败： {"ok":false,"error":"..."}
 */
char *dm_peers_json(dm_handle *handle);

/*
 * 软件目录（只读展示）。
 * 成功： {"ok":true,"revision":3,"entries":[
 *          {"id":"ent-...","name":"...","version":"1.0.0","platform":"windows",
 *           "kind":"installer","size":1024,"tags":["工具"],"notes":"",
 *           "updated_at_ms":1700000000000,"updated_by":"dev-...",
 *           "local":true,"holders":["dev-..."]}],
 *         "count":1}
 * 失败： {"ok":false,"error":"..."}
 */
char *dm_catalog_json(dm_handle *handle);

/*
 * 会话运行状态（UI 轮询/状态栏用）。
 * {"ok":true,"running":true,"active":true,"device_id":"dev-...","name":"...",
 *  "relay_url":"ws://...","data_dir":"/data/...","peers_online":1,
 *  "pairing":false,"last_clipboard_len":12}
 */
char *dm_status_json(dm_handle *handle);

/* 释放本库返回的字符串（NULL 安全）。 */
void dm_string_free(char *s);

/* 线程本地最近一次错误（静态字符串，不要释放）；无错误时返回 NULL。 */
const char *dm_last_error(void);

/*
 * ============================ 事件 JSON ============================
 * 每个事件是 {"type":"...", ...}。type 取值：
 *
 *   started             {"device_id":"dev-...","name":"..."}
 *   stopped             {}
 *   connected           {"device_id":"dev-...","online":["dev-..."]}
 *   presence            {"online":["dev-..."]}
 *   peer_online         {"peer_id":"dev-...","name":"..."}
 *   peer_offline        {"peer_id":"dev-..."}
 *   session_established {"peer_id":"dev-...","name":"..."}
 *   session_closed      {"peer_id":"dev-..."}
 *   clipboard_captured  {"kind":"text","size":12}
 *   clipboard_sent      {"peers":1,"kind":"text","size":12}
 *   clipboard_applied   {"peer_id":"dev-...","kind":"text","size":12}   内容另走 dm_clipboard_cb
 *   clipboard_spooled   {"peers":1}
 *   catalog_updated     {"entry_id":"ent-...","ops":1}
 *   catalog_merged      {"from":"dev-...","applied":1,"ignored":0,"conflicts":0}
 *   peers_reloaded      {"count":1}
 *   pair_code           {"code":"12345678","qr":"dsync-pair-v1:..."}
 *   pair_peer           {"peer_id":"dev-...","name":"..."}
 *   pair_succeeded      {"peer_id":"dev-...","name":"...","sas":"123456"}
 *   pair_failed         {"message":"..."}
 *   transfer_received   {"entry_id":"ent-...","path":"/data/...","size":1024}
 *   transfer_failed     {"transfer_id":"...","message":"..."}
 *   relay_error         {"code":"...","message":"..."}
 *   error               {"message":"..."}
 *   paused              {"paused":true}
 *
 * 说明：事件集合可能随版本增量扩展（只增不改）；未知 type 请忽略。
 * 完整的 type 列表还包括 spool_replayed / catalog_reloaded / peer_have_list /
 * transfer_offered / transfer_started / transfer_progress / vault_shared /
 * vault_key_received / peer_revoked / peer_unpaired / event（降级 summary）等。
 */
#ifdef __cplusplus
}
#endif

#endif /* DRIVE_MOBILE_H */
