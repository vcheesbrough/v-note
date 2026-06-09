use gloo_net::http::Request;
use leptos::prelude::*;
use protocol::{MeResponse, MetaResponse};

#[component]
fn App() -> impl IntoView {
    let meta = RwSignal::new(None::<Result<MetaResponse, String>>);
    let me = RwSignal::new(None::<Option<Result<MeResponse, u16>>>);

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            let meta_result = async {
                let response = Request::get("/api/meta")
                    .send()
                    .await
                    .map_err(|error| format!("request failed: {error}"))?;
                response
                    .json::<MetaResponse>()
                    .await
                    .map_err(|error| format!("invalid response JSON: {error}"))
            }
            .await;
            meta.set(Some(meta_result));

            let me_result = Request::get("/api/me").send().await;
            me.set(Some(match me_result {
                Ok(response) if response.ok() => {
                    match response.json::<MeResponse>().await {
                        Ok(profile) => Some(Ok(profile)),
                        Err(_) => Some(Err(500)),
                    }
                }
                Ok(response) => Some(Err(response.status())),
                Err(_) => Some(Err(0)),
            }));
        });
    });

    view! {
        <main style="font-family: system-ui, sans-serif; padding: 2rem; max-width: 40rem;">
            <header style="display: flex; justify-content: space-between; align-items: center; gap: 1rem;">
                <h1 style="margin: 0;">"v-note"</h1>
                {move || match me.get() {
                    None => view! { <span>"Checking session…"</span> }.into_any(),
                    Some(Some(Ok(profile))) => view! {
                        <div style="display: flex; gap: 1rem; align-items: center;">
                            <span>{profile.email.clone().unwrap_or_else(|| profile.sub.clone())}</span>
                            <a href="/auth/logout">"Sign out"</a>
                        </div>
                    }
                    .into_any(),
                    Some(Some(Err(401))) | Some(Some(Err(403))) => view! {
                        <a href="/auth/login">"Sign in"</a>
                    }
                    .into_any(),
                    Some(Some(Err(_))) => view! {
                        <span>"Session check failed"</span>
                    }
                    .into_any(),
                    Some(None) => view! { <span>"Session unavailable"</span> }.into_any(),
                }}
            </header>

            {move || match me.get() {
                None => view! { <p>"Loading…"</p> }.into_any(),
                Some(Some(Ok(_))) => view! {
                    <section>
                        <p>"Signed in. Page library and ink capture land in later iterations."</p>
                        {move || match meta.get() {
                            None => view! { <p>"Loading metadata…"</p> }.into_any(),
                            Some(Ok(info)) => view! {
                                <p>{format!("Version {}", info.app_version)}</p>
                                <p>{format!("Protocol {}", info.protocol_version)}</p>
                            }
                            .into_any(),
                            Some(Err(error)) => view! {
                                <p>{format!("Failed to load metadata: {error}")}</p>
                            }
                            .into_any(),
                        }}
                    </section>
                }
                .into_any(),
                Some(Some(Err(401))) | Some(Some(Err(403))) => view! {
                    <section>
                        <p>"Sign in with Authentik to use v-note."</p>
                        <p><a href="/auth/login">"Continue to sign in"</a></p>
                    </section>
                }
                .into_any(),
                _ => view! {
                    <p>"Unable to determine authentication state."</p>
                }
                .into_any(),
            }}

            <footer style="margin-top: 2rem; font-size: 0.875rem;">
                <a href="/dl/apk">"Download Android app (.apk)"</a>
            </footer>
        </main>

        // Version watermark — persistent build identity (server release via /api/meta).
        {move || {
            meta.get().and_then(|result| result.ok()).map(|info| view! {
                <div style="position: fixed; bottom: 0.5rem; right: 0.75rem; opacity: 0.35; \
                    font-size: 0.75rem; pointer-events: none; user-select: none; \
                    font-family: system-ui, sans-serif;">
                    {format!("v{}", info.app_version)}
                </div>
            })
        }}
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
