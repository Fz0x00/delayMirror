use worker::{event, Context, Env, Request, Response};

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    console_error_panic_hook::set_once();

    let config =
        delay_mirror::core::Config::from_env_vars(|key| env.var(key).ok().map(|v| v.to_string()));
    let checker = match delay_mirror::core::DelayChecker::with_delay_days(config.delay_days) {
        Ok(c) => c,
        Err(e) => {
            return Response::error(
                serde_json::json!({
                    "error": "Invalid configuration",
                    "details": e.to_string()
                })
                .to_string(),
                500,
            );
        }
    };

    delay_mirror::workers::router::route_request(req, &config, &checker).await
}
