//! Which WebView2 renderer processes hold the dashboard (not the overlay), for its below-normal priority during champ
//! select and games (0.14.0, desktop/main.ts dashboardPriority: Electron lowered the dashboard's renderer only). WebView2
//! lists its processes with the frames each one holds (ICoreWebView2Environment13::GetProcessExtendedInfos); a renderer
//! holding the overlay is left at normal priority, so the HUD keeps up during the game.

#[cfg(windows)]
pub fn dashboard_renderers(main: &tauri::WebviewWindow, origin: String, done: impl FnOnce(Vec<u32>) + Send + 'static) {
    use std::sync::{Arc, Mutex};
    use webview2_com::GetProcessExtendedInfosCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Environment13, COREWEBVIEW2_PROCESS_KIND, COREWEBVIEW2_PROCESS_KIND_RENDERER};
    use windows_core::{Interface, BOOL, PWSTR};

    let done: Arc<Mutex<Option<Box<dyn FnOnce(Vec<u32>) + Send>>>> = Arc::new(Mutex::new(Some(Box::new(done))));
    let finish = move |d: &Arc<Mutex<Option<Box<dyn FnOnce(Vec<u32>) + Send>>>>, pids: Vec<u32>| {
        if let Some(f) = d.lock().unwrap().take() {
            f(pids);
        }
    };
    let d2 = done.clone();
    let r = main.with_webview(move |pw| {
        let env = pw.environment();
        let Ok(env13) = env.cast::<ICoreWebView2Environment13>() else {
            finish(&d2, vec![]);
            return;
        };
        let d3 = d2.clone();
        let overlay = format!("{origin}/overlay");
        let handler = GetProcessExtendedInfosCompletedHandler::create(Box::new(move |hr, infos| {
            let mut pids = vec![];
            if let (Ok(()), Some(infos)) = (hr, infos) {
                unsafe {
                    let mut n = 0u32;
                    let _ = infos.Count(&mut n);
                    for i in 0..n {
                        let Ok(info) = infos.GetValueAtIndex(i) else { continue };
                        let Ok(p) = info.ProcessInfo() else { continue };
                        let mut kind = COREWEBVIEW2_PROCESS_KIND::default();
                        let _ = p.Kind(&mut kind);
                        if kind != COREWEBVIEW2_PROCESS_KIND_RENDERER {
                            continue;
                        }
                        let mut pid = 0i32;
                        let _ = p.ProcessId(&mut pid);
                        let Ok(frames) = info.AssociatedFrameInfos() else { continue };
                        let Ok(it) = frames.GetIterator() else { continue };
                        let (mut any, mut overlay_here) = (false, false);
                        loop {
                            let mut has = BOOL::default();
                            if it.HasCurrent(&mut has).is_err() || !has.as_bool() {
                                break;
                            }
                            if let Ok(f) = it.GetCurrent() {
                                let mut src = PWSTR::null();
                                if f.Source(&mut src).is_ok() && !src.is_null() {
                                    let s = src.to_string().unwrap_or_default();
                                    windows::Win32::System::Com::CoTaskMemFree(Some(src.as_ptr() as *const _));
                                    any = true;
                                    if s.starts_with(&overlay) {
                                        overlay_here = true;
                                    }
                                }
                            }
                            let mut next = BOOL::default();
                            if it.MoveNext(&mut next).is_err() {
                                break;
                            }
                        }
                        if any && !overlay_here && pid > 0 {
                            pids.push(pid as u32);
                        }
                    }
                }
            }
            finish(&d3, pids);
            Ok(())
        }));
        if unsafe { env13.GetProcessExtendedInfos(&handler) }.is_err() {
            finish(&d2, vec![]);
        }
    });
    if r.is_err() {
        if let Some(f) = done.lock().unwrap().take() {
            f(vec![]);
        }
    }
}
