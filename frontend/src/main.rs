use gloo_net::http::Request;
use leptos::prelude::*;
use protocol::MetaResponse;

#[component]
fn App() -> impl IntoView {
    let meta = RwSignal::new(None::<Result<MetaResponse, String>>);

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            let result = async {
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

            meta.set(Some(result));
        });
    });

    view! {
        <main>
            <h1>"v-note"</h1>
            {move || match meta.get() {
                None => view! { <p>"Loading metadata..."</p> }.into_any(),
                Some(Ok(info)) => view! {
                    <p>{format!("Version {}", info.app_version)}</p>
                    <p>{format!("Protocol {}", info.protocol_version)}</p>
                }
                .into_any(),
                Some(Err(error)) => view! { <p>{format!("Failed to load metadata: {error}")}</p> }
                    .into_any(),
            }}
        </main>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
