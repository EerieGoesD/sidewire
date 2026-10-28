// Android only: 15-day free trial, then one purchase on Google Play for
// lifetime use. Copied from Bear in Mind (bear-in-mind/src-tauri/src/licensing.rs)
// and app_related/android/google_app_billing_models.txt, FREE TRIAL + ONE-TIME
// UNLOCK ON ANDROID. One addition: the Remote subscription unlocks everything
// too, so a subscriber never meets the trial at all.
//
// THE TRIAL DATE. Play has a minimum price for every one-time product, so there
// is no free item to hang the date on, and a date kept on the phone is gone
// after a reinstall or "Clear data". So the date lives on the EERIE Worker
// (eerie-cloud-sync, /trial/sidewire), keyed by a SHA-256 of the phone's
// Android ID. The Android ID survives a reinstall and "Clear data"; it changes
// only on a factory reset. The ID itself never leaves the phone, only its hash.
// The date is the Worker's clock, and the time since is the Worker's clock plus
// a monotonic timer, so moving the phone's clock back does nothing.
//
// The unlock is a one-time, non-consumable product. Play is the source of
// truth for it and for the subscription, and is asked again on every launch.
// license.json is only a cache of the answers. A purchase is acknowledged as
// soon as it is seen and is never consumed.
//
// NO WAY ROUND IT OFFLINE. When Play or the Worker cannot be reached, the gate
// does not open up: it judges from the last answers it kept (license.json),
// with "now" never earlier than the latest time it has ever seen, so winding
// the phone's clock back while offline buys nothing. A paying user who was
// online once stays unlocked offline. Only one case is left open, and only in a
// TEST build: Play answering that the unlock does not exist, which is the setup
// phase before it is created in Play Console. The Play build never opens on
// that answer: a copy re-signed by someone else gets the very same answer from
// Play, so there it is judged like offline (see evaluate).
//
// Off Android every command here is inert and there is no trial and no gate.

#[cfg(target_os = "android")]
use std::path::PathBuf;
#[cfg(target_os = "android")]
use std::sync::Mutex;
#[cfg(target_os = "android")]
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
use serde::Deserialize;
use serde::Serialize;
use tauri::AppHandle;

// Must match the product ID created in Play Console exactly.
#[cfg(target_os = "android")]
const UNLOCK_ID: &str = "com.eeriegoesd.sidewire.unlock";

// A one-time product; the plugin calls that type "inapp".
#[cfg(target_os = "android")]
const INAPP: &str = "inapp";

// The Remote subscription (REMOTE_PRODUCT_ID in App.tsx). Holding it unlocks
// the whole app as well.
#[cfg(target_os = "android")]
const REMOTE_SUB_ID: &str = "sidewire_remote_monthly";
#[cfg(target_os = "android")]
const SUBS: &str = "subs";

// Where the trial's start date lives. The last part is this app's name there.
#[cfg(target_os = "android")]
const TRIAL_URL: &str = "https://eerie-apps-sync.eeriegoesd.workers.dev/trial/sidewire";

// Trial length in ms. MUST match TRIAL_DAYS in License.tsx. 15 days, so it
// outlasts Google's 14-day closed test.
#[cfg(target_os = "android")]
const TRIAL_MS: i64 = 15 * 86_400_000;

// How long one answer from Play and the Worker is trusted before the gate asks
// again. Launch and every return to the app ask anyway, so sending almost
// never waits on the network.
#[cfg(target_os = "android")]
const FRESH: Duration = Duration::from_secs(10 * 60);

// The same strings are defined in License.tsx; keep the two identical.
#[cfg(target_os = "android")]
const STORE_UNREACHABLE: &str =
    "Can't connect to Google Play. Check your internet connection and try again.";
#[cfg(not(target_os = "android"))]
const NOT_STORE_BUILD: &str = "Purchases only work in the Google Play version of SideWire.";
pub const TRIAL_ENDED: &str = "Your free trial has ended. Buy SideWire to keep using it.";

#[cfg(target_os = "android")]
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(target_os = "android")]
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct License {
    // The unlock bought, or the Remote subscription active.
    pub owned: bool,
    // 0 means the trial has never been started.
    pub trial_start_ms: i64,
    // The latest time ever seen: the Worker's clock when it last answered, or
    // the phone's clock offline if that is later. Offline, "now" is never
    // earlier than this, so the clock cannot be wound back past it.
    pub max_seen_ms: i64,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub owned: bool,
    // Paid with a method that clears later (cash at a shop, some bank
    // transfers). Not owned until Play says so.
    pub pending: bool,
    pub trial_start_ms: i64,
    // The Worker's clock when it answered (offline: the latest time ever
    // seen), so the page counts the days left on that and not on the phone's
    // clock.
    pub now_ms: i64,
    // True when Play and the Worker both answered just now.
    pub store_ready: bool,
    // True when Play answered that the unlock does not exist yet (before it is
    // created in Play Console), in a test build only. The only state the page
    // leaves open; the Play build never reports it.
    pub setup: bool,
}

// What one look at Play and the Worker found.
#[cfg(target_os = "android")]
enum Reach {
    // Both answered.
    Online(Verified),
    // Play answered: the unlock does not exist yet. Test builds only.
    Setup,
    // Play or the Worker could not be reached.
    Offline,
}

// The one rule, online or off: owned, or the trial not started yet (the start
// screen covers the app then), or started less than 15 days ago.
#[cfg(target_os = "android")]
fn allowed(owned: bool, trial_start_ms: i64, now: i64) -> bool {
    owned || trial_start_ms == 0 || now < trial_start_ms + TRIAL_MS
}

// The last time both Play and the Worker answered.
#[cfg(target_os = "android")]
#[derive(Clone, Copy)]
struct Verified {
    owned: bool,
    pending: bool,
    trial_start_ms: i64,
    server_now_ms: i64,
    at: Instant,
}

#[cfg(target_os = "android")]
static LAST: Mutex<Option<Verified>> = Mutex::new(None);

// The Worker's clock now: what it said, plus the time since on a timer the
// phone's clock setting cannot move.
#[cfg(target_os = "android")]
fn server_now(v: &Verified) -> i64 {
    v.server_now_ms + v.at.elapsed().as_millis() as i64
}

#[cfg(target_os = "android")]
fn license_file(app: &AppHandle) -> PathBuf {
    crate::data_dir(app).join("license.json")
}

#[cfg(target_os = "android")]
fn read_license(app: &AppHandle) -> License {
    std::fs::read(license_file(app))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

// Best-effort: a cache that fails to write costs a round trip on the next
// launch, nothing more.
#[cfg(target_os = "android")]
fn write_license(app: &AppHandle, lic: &License) {
    if let Ok(json) = serde_json::to_vec_pretty(lic) {
        let _ = std::fs::write(license_file(app), json);
    }
}

// Settings.Secure.ANDROID_ID, read through JNI on the activity's thread. Since
// Android 8 it is one value per signing key, user and phone, kept through
// reinstalls and "Clear data", new only after a factory reset.
#[cfg(target_os = "android")]
fn android_id(app: &AppHandle) -> Option<String> {
    use jni::objects::{JString, JValue};
    use tauri::Manager;

    let (tx, rx) = std::sync::mpsc::channel();
    app.get_webview_window("main")?
        .with_webview(move |platform| {
            platform.jni_handle().exec(move |env, activity, _webview| {
                let mut read = || -> jni::errors::Result<String> {
                    let resolver = env
                        .call_method(
                            activity,
                            "getContentResolver",
                            "()Landroid/content/ContentResolver;",
                            &[],
                        )?
                        .l()?;
                    let name = env.new_string("android_id")?;
                    let value = env
                        .call_static_method(
                            "android/provider/Settings$Secure",
                            "getString",
                            "(Landroid/content/ContentResolver;Ljava/lang/String;)Ljava/lang/String;",
                            &[JValue::Object(&resolver), JValue::Object(&name)],
                        )?
                        .l()?;
                    if value.is_null() {
                        return Ok(String::new());
                    }
                    let value = JString::from(value);
                    let text: String = env.get_string(&value)?.into();
                    Ok(text)
                };
                let id = read();
                if id.is_err() {
                    let _ = env.exception_clear();
                }
                let _ = tx.send(id.ok());
            });
        })
        .ok()?;
    rx.recv_timeout(Duration::from_secs(5))
        .ok()
        .flatten()
        .filter(|id| !id.is_empty())
}

// What the Worker gets instead of the Android ID: a SHA-256 of it, salted with
// the app's name, in hex.
#[cfg(target_os = "android")]
fn device_key(app: &AppHandle) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::sync::OnceLock;

    static KEY: OnceLock<String> = OnceLock::new();
    if let Some(key) = KEY.get() {
        return Some(key.clone());
    }
    let id = android_id(app)?;
    let digest = Sha256::digest(format!("sidewire|{id}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Some(KEY.get_or_init(|| hex).clone())
}

#[cfg(target_os = "android")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrialAnswer {
    started_at: Option<i64>,
    now: i64,
}

// GET reads the start date (none yet: null); POST starts the trial, or hands
// back the first date if it was started before.
#[cfg(target_os = "android")]
async fn trial_server(device: String, start: bool) -> Result<TrialAnswer, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(6)))
                .build(),
        );
        let mut resp = if start {
            agent
                .post(TRIAL_URL)
                .header("content-type", "application/json")
                .send(format!("{{\"device\":\"{device}\"}}"))
        } else {
            agent.get(format!("{TRIAL_URL}?device={device}")).call()
        }
        .map_err(|e| e.to_string())?;
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        serde_json::from_str::<TrialAnswer>(&text).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// One product's state on Play: (purchased, pending). Acknowledges a purchase
// Play has not had acknowledged yet: Play refunds one that never is, and this
// runs on every launch, so a purchase that cleared while the app was closed is
// acknowledged the next time it opens.
#[cfg(target_os = "android")]
async fn play_product(app: &AppHandle, id: &str, kind: &str) -> Result<(bool, bool), String> {
    use tauri_plugin_iap::{IapExt, PurchaseStateValue};

    let status = app
        .iap()
        .get_product_status(id.to_string(), kind.to_string())
        .await
        .map_err(|e| e.to_string())?;

    // The plugin reports a pending purchase as "owned" too, so the state is
    // what decides.
    let purchased =
        status.is_owned && status.purchase_state == Some(PurchaseStateValue::Purchased);
    let pending = status.is_owned && status.purchase_state == Some(PurchaseStateValue::Pending);

    if purchased && status.is_acknowledged == Some(false) {
        if let Some(token) = status.purchase_token.clone() {
            let _ = app.iap().acknowledge_purchase(token).await;
        }
    }
    Ok((purchased, pending))
}

// Ask Play about the unlock and the subscription. Returns (owned, pending):
// owned when either is held, pending for the unlock.
#[cfg(target_os = "android")]
async fn play_owned(app: &AppHandle) -> Result<(bool, bool), String> {
    let (unlocked, pending) = play_product(app, UNLOCK_ID, INAPP).await?;
    let subscribed = if unlocked {
        true
    } else {
        play_product(app, REMOTE_SUB_ID, SUBS).await?.0
    };
    let owned = unlocked || subscribed;

    let mut lic = read_license(app);
    lic.owned = owned;
    write_license(app, &lic);
    if let Ok(mut last) = LAST.lock() {
        if let Some(v) = last.as_mut() {
            v.owned = owned;
            v.pending = pending;
        }
    }
    Ok((owned, pending))
}

// Ask Play and the Worker, cache both answers, and remember when. Writes
// nothing unless both answered, so a failed read never overwrites a paying
// user's record with "nothing bought".
#[cfg(target_os = "android")]
async fn refresh(app: &AppHandle) -> Result<Verified, String> {
    let (owned, pending) = play_owned(app).await?;
    let device = device_key(app).ok_or_else(|| "no device key".to_string())?;
    let trial = trial_server(device, false).await?;

    let v = Verified {
        owned,
        pending,
        trial_start_ms: trial.started_at.unwrap_or(0),
        server_now_ms: trial.now,
        at: Instant::now(),
    };
    // The Worker's clock is the truth for "now". It replaces whatever the
    // phone's clock pushed max_seen to, so a clock once set wrongly forward
    // stops counting against the user the next time they are online.
    write_license(
        app,
        &License {
            owned,
            trial_start_ms: v.trial_start_ms,
            max_seen_ms: trial.now,
        },
    );
    if let Ok(mut last) = LAST.lock() {
        *last = Some(v);
    }
    Ok(v)
}

// Ok(true): Play knows the unlock product. Ok(false): Play answered and does
// not know it (the setup phase). Err: Play could not be reached. Play answers
// "not owned" for a product that does not exist, so only this check can tell
// "not bought" from "not set up".
#[cfg(target_os = "android")]
async fn product_known(app: &AppHandle) -> Result<bool, String> {
    use tauri_plugin_iap::IapExt;
    let resp = app
        .iap()
        .get_products(vec![UNLOCK_ID.to_string()], INAPP.to_string())
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp.products.iter().any(|p| p.product_id == UNLOCK_ID))
}

#[cfg(target_os = "android")]
async fn evaluate(app: &AppHandle) -> Reach {
    match product_known(app).await {
        Ok(true) => match refresh(app).await {
            Ok(v) => Reach::Online(v),
            Err(_) => Reach::Offline,
        },
        // Play answered that the unlock does not exist. A test build (debug)
        // stays open, since the product may not be in Play Console yet. The
        // Play build (release) does not: a re-signed copy of the app gets this
        // same answer, and opening on it would hand the app away to anyone
        // who re-signs it. There it is judged like offline, from what this
        // phone has kept: a trial to start, then the paywall.
        Ok(false) if cfg!(debug_assertions) => Reach::Setup,
        Ok(false) => Reach::Offline,
        Err(_) => Reach::Offline,
    }
}

// Offline: the last answers kept, and "now" as the later of the phone's clock
// and the latest time ever seen. Moves the latest time forward as it goes.
// Returns (owned, trial_start_ms, now).
#[cfg(target_os = "android")]
fn offline_view(app: &AppHandle) -> (bool, i64, i64) {
    let mut lic = read_license(app);
    let now = lic.max_seen_ms.max(now_ms());
    if now != lic.max_seen_ms {
        lic.max_seen_ms = now;
        write_license(app, &lic);
    }
    (lic.owned, lic.trial_start_ms, now)
}

// True when the user may use the app (see allowed). Uses the last online
// answer while it is fresh, so sending does not wait on the network. Offline
// it judges from the kept answers instead of opening up. Open only in the
// setup phase, when Play says the unlock does not exist, and only in a test
// build.
#[cfg(target_os = "android")]
pub async fn has_access(app: &AppHandle) -> bool {
    let recent = LAST
        .lock()
        .ok()
        .and_then(|last| *last)
        .filter(|v| v.at.elapsed() < FRESH);
    if let Some(v) = recent {
        return allowed(v.owned, v.trial_start_ms, server_now(&v));
    }
    match evaluate(app).await {
        Reach::Online(v) => allowed(v.owned, v.trial_start_ms, server_now(&v)),
        Reach::Setup => true,
        Reach::Offline => {
            let (owned, start, now) = offline_view(app);
            allowed(owned, start, now)
        }
    }
}

// Off Android there is no gate.
#[cfg(not(target_os = "android"))]
pub async fn has_access(_app: &AppHandle) -> bool {
    true
}

#[tauri::command]
pub async fn iap_status(app: AppHandle) -> Result<Status, String> {
    #[cfg(target_os = "android")]
    {
        Ok(match evaluate(&app).await {
            Reach::Online(v) => Status {
                owned: v.owned,
                pending: v.pending,
                trial_start_ms: v.trial_start_ms,
                now_ms: server_now(&v),
                store_ready: true,
                setup: false,
            },
            // Nobody can have bought a product that does not exist yet.
            Reach::Setup => Status {
                owned: false,
                pending: false,
                trial_start_ms: 0,
                now_ms: now_ms(),
                store_ready: false,
                setup: true,
            },
            // The kept answers, judged exactly as online ones are.
            Reach::Offline => {
                let (owned, trial_start_ms, now_ms) = offline_view(&app);
                Status {
                    owned,
                    pending: false,
                    trial_start_ms,
                    now_ms,
                    store_ready: false,
                    setup: false,
                }
            }
        })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = &app;
        Ok(Status {
            owned: true,
            pending: false,
            trial_start_ms: 0,
            now_ms: 0,
            store_ready: false,
            setup: false,
        })
    }
}

// The unlock price as Play formats it for this user's country. Never
// hardcoded: the same product is a different amount and currency per country.
#[tauri::command]
pub async fn iap_price(app: AppHandle) -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        use tauri_plugin_iap::IapExt;
        let resp = app
            .iap()
            .get_products(vec![UNLOCK_ID.to_string()], INAPP.to_string())
            .await
            .map_err(|e| e.to_string())?;
        Ok(resp
            .products
            .into_iter()
            .find(|p| p.product_id == UNLOCK_ID)
            .and_then(|p| p.formatted_price)
            .unwrap_or_default())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = &app;
        Ok(String::new())
    }
}

// Starts the trial on the Worker, the first time. If this phone started one
// before (another install, cleared data), the Worker hands back that first
// date instead, so tapping "Start Free Trial" again never buys more days.
#[tauri::command]
pub async fn iap_start_trial(app: AppHandle) -> Result<i64, String> {
    #[cfg(target_os = "android")]
    {
        let device = device_key(&app).ok_or_else(|| STORE_UNREACHABLE.to_string())?;
        let answer = trial_server(device, true)
            .await
            .map_err(|_| STORE_UNREACHABLE.to_string())?;
        let start = answer.started_at.unwrap_or(answer.now);

        let mut lic = read_license(&app);
        lic.trial_start_ms = start;
        lic.max_seen_ms = answer.now;
        write_license(&app, &lic);
        if let Ok(mut last) = LAST.lock() {
            if let Some(v) = last.as_mut() {
                v.trial_start_ms = start;
                v.server_now_ms = answer.now;
                v.at = Instant::now();
            }
        }
        Ok(start)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = &app;
        Err(NOT_STORE_BUILD.to_string())
    }
}

// Opens Play's purchase sheet for the unlock. Resolves once Play reports the
// purchase as paid; a cancel comes back as an error. A pending purchase never
// resolves here: the page re-reads the status when the app comes back to the
// front, and that is where pending shows up.
#[tauri::command]
pub async fn iap_buy(app: AppHandle) -> Result<bool, String> {
    #[cfg(target_os = "android")]
    {
        use tauri_plugin_iap::{IapExt, PurchaseRequest};
        let bought = app
            .iap()
            .purchase(PurchaseRequest {
                product_id: UNLOCK_ID.to_string(),
                product_type: INAPP.to_string(),
                options: None,
            })
            .await
            .map_err(|e| e.to_string())?;

        if !bought.is_acknowledged {
            let _ = app
                .iap()
                .acknowledge_purchase(bought.purchase_token.clone())
                .await;
        }
        let (owned, _) = play_owned(&app)
            .await
            .map_err(|_| STORE_UNREACHABLE.to_string())?;
        Ok(owned)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = &app;
        Err(NOT_STORE_BUILD.to_string())
    }
}

// For a reinstall or a new phone. Never trusts the cache: the answer comes
// from Play, so editing license.json cannot make "owned" true. If Play does
// not answer it is an error, never a quiet success.
#[tauri::command]
pub async fn iap_restore(app: AppHandle) -> Result<bool, String> {
    #[cfg(target_os = "android")]
    {
        use tauri_plugin_iap::IapExt;
        app.iap()
            .restore_purchases(INAPP.to_string())
            .await
            .map_err(|_| STORE_UNREACHABLE.to_string())?;
        let (owned, _) = play_owned(&app)
            .await
            .map_err(|_| STORE_UNREACHABLE.to_string())?;
        Ok(owned)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = &app;
        Err(NOT_STORE_BUILD.to_string())
    }
}
