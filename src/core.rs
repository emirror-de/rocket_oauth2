use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Debug;
use std::path::PathBuf;

use rocket::config::{self, Config, ConfigError, Table, Value};
use rocket::fairing::{AdHoc, Fairing};
use rocket::handler;
use rocket::http::uri::Absolute;
use rocket::http::{Cookie, Cookies, Method, SameSite, Status};
use rocket::outcome::{IntoOutcome, Outcome};
use rocket::request::{FormItems, FromForm, Request};
use rocket::response::{Redirect, Responder};
use rocket::{Data, FromForm, Route, State};
use serde_json::Value as JsonValue;

const STATE_COOKIE_NAME: &str = "rocket_oauth2_state";

/// The server's response to a successful token exchange, defined in
/// in RFC 6749 §5.1.
#[derive(serde::Deserialize)]
pub struct TokenResponse {
    /// The access token issued by the authorization server.
    pub access_token: String,
    /// The type of token, described in RFC 6749 §7.1.
    pub token_type: String,
    /// The lifetime in seconds of the access token, if the authorization server
    /// provided one.
    pub expires_in: Option<i32>,
    /// The refresh token, if the server provided one.
    pub refresh_token: Option<String>,
    /// The (space-separated) list of scopes associated with the access token.
    /// The authorization server is required to provide this if it differs from
    /// the requested set of scopes.
    pub scope: Option<String>,

    /// Additional values returned by the authorization server, if any.
    #[serde(flatten)]
    pub extras: HashMap<String, JsonValue>,
}

/// An OAuth2 `Adapater` can be implemented by any type that facilitates the
/// Authorization Code Grant as described in RFC 6749 §4.1. The implementing
/// type must be able to generate an authorization URI and perform the token
/// exchange.
pub trait Adapter: Send + Sync + 'static {
    /// The `Error` type returned by this `Adapter` when a URI generation or
    /// token exchange fails.
    type Error: Debug;

    /// Generate an authorization URI and state value as described by RFC 6749 §4.1.1.
    fn authorization_uri(
        &self,
        config: &OAuthConfig,
        scopes: &[&str],
    ) -> Result<(Absolute<'static>, String), Self::Error>;

    /// Perform the token exchange in accordance with RFC 6749 §4.1.3 given the
    /// authorization code provided by the service.
    fn exchange_code(&self, config: &OAuthConfig, code: &str)
        -> Result<TokenResponse, Self::Error>;
}

/// An OAuth2 `Callback` implements application-specific OAuth client logic,
/// such as setting login cookies and making database and API requests. It is
/// tied to a specific `Adapter`, and will recieve an instance of the Adapter's
/// `Token` type.
pub trait Callback: Send + Sync + 'static {
    // TODO: Relax 'static. Would this need GAT/ATC?
    /// The callback Responder type.
    type Responder: Responder<'static>;

    /// This method will be called when a token exchange has successfully
    /// completed and will be provided with the request and the token.
    /// Implementors should perform application-specific logic here, such as
    /// checking a database or setting a login cookie.
    fn callback(&self, request: &Request<'_>, token: TokenResponse) -> Self::Responder;
}

impl<F, R> Callback for F
where
    F: Fn(&Request<'_>, TokenResponse) -> R + Send + Sync + 'static,
    R: Responder<'static>,
{
    type Responder = R;

    fn callback(&self, request: &Request<'_>, token: TokenResponse) -> Self::Responder {
        (self)(request, token)
    }
}

/// Holds configuration for an OAuth application. This consists of the [Provider]
/// details, a `client_id` and `client_secret`, and a `redirect_uri`.
pub struct OAuthConfig {
    provider: Provider,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
}

fn get_config_string(table: &Table, key: &str) -> config::Result<String> {
    let value = table
        .get(key)
        .ok_or_else(|| ConfigError::Missing(key.into()))?;

    let string = value
        .as_str()
        .ok_or_else(|| ConfigError::BadType(key.into(), "string", value.type_str(), "".into()))?;

    Ok(string.to_string())
}

impl OAuthConfig {
    /// Create a new OAuthConfig.
    pub fn new(
        provider: Provider,
        client_id: String,
        client_secret: String,
        redirect_uri: String,
    ) -> OAuthConfig {
        OAuthConfig {
            provider,
            client_id,
            client_secret,
            redirect_uri,
        }
    }

    /// Constructs a OAuthConfig from Rocket configuration
    pub fn from_config(config: &Config, name: &str) -> config::Result<OAuthConfig> {
        let oauth = config.get_table("oauth")?;
        let conf = oauth
            .get(name)
            .ok_or_else(|| ConfigError::Missing(name.to_string()))?;

        let table = conf.as_table().ok_or_else(|| {
            ConfigError::BadType(name.into(), "table", conf.type_str(), "".into())
        })?;

        let provider = match conf.get("provider") {
            Some(v) => Provider::from_config_value(v),
            None => Err(ConfigError::Missing("provider".to_string())),
        }?;

        let client_id = get_config_string(table, "client_id")?;
        let client_secret = get_config_string(table, "client_secret")?;
        let redirect_uri = get_config_string(table, "redirect_uri")?;

        Ok(OAuthConfig::new(
            provider,
            client_id,
            client_secret,
            redirect_uri,
        ))
    }

    /// Gets the [Provider] for this configuration.
    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// Gets the client id for this configuration.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Gets the client secret for this configuration.
    pub fn client_secret(&self) -> &str {
        &self.client_secret
    }

    /// Gets the redirect URI for this configuration.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }
}

/// The `OAuth2` structure implements OAuth in a Rocket application by setting
/// up OAuth-related route handlers.
///
/// ## Redirect handler
/// `OAuth2` handles the redirect URI. It verifies the `state` token to prevent
/// CSRF attacks, then instructs the Adapter to perform the token exchange. The
/// resulting token is passed to the `Callback`.
///
/// ## Login handler
/// `OAuth2` optionally handles a login route, which simply redirects to the
/// authorization URI generated by the `Adapter`. Whether or not `OAuth2` is
/// handling a login URI, `get_redirect` can be used to get a `Redirect` to the
/// OAuth login flow manually.
pub struct OAuth2<A, C> {
    adapter: A,
    callback: C,
    config: OAuthConfig,
    default_scopes: Vec<String>,
}

impl<A: Adapter, C: Callback> OAuth2<A, C> {
    /// Returns an OAuth2 fairing. The fairing will place an instance of
    /// `OAuth2<A, C>` in managed state and mount a redirect handler. It will
    /// also mount a login handler if `login` is `Some`.
    pub fn fairing<CN, CU, LU, LS>(
        adapter: A,
        callback: C,
        config_name: CN,
        callback_uri: CU,
        login: Option<(LU, Vec<LS>)>,
    ) -> impl Fairing
    where
        CN: Into<Cow<'static, str>>,
        CU: Into<Cow<'static, str>>,
        LU: Into<Cow<'static, str>>,
        LS: Into<String>,
    {
        let config_name = config_name.into();
        let callback_uri = callback_uri.into();
        let mut login = login.map(|login| {
            (
                login.0.into(),
                login.1.into_iter().map(Into::into).collect(),
            )
        });
        AdHoc::on_attach("OAuth Init", move |rocket| {
            let config = match OAuthConfig::from_config(rocket.config(), &config_name) {
                Ok(c) => c,
                Err(e) => {
                    log::error!("Invalid configuration: {:?}", e);
                    return Err(rocket);
                }
            };

            let login = login
                .as_mut()
                .map(|l: &mut (Cow<'static, str>, Vec<String>)| {
                    (l.0.as_ref(), l.1.drain(..).collect())
                });

            Ok(rocket.attach(Self::custom(
                adapter,
                callback,
                config,
                &callback_uri,
                login,
            )))
        })
    }

    /// Returns an OAuth2 fairing with custom configuration. The fairing will
    /// place an instance of `OAuth2<A, C>` in managed state and mount a
    /// redirect handler. It will also mount a login handler if `login` is
    /// `Some`.
    pub fn custom(
        adapter: A,
        callback: C,
        config: OAuthConfig,
        callback_uri: &str,
        login: Option<(&str, Vec<String>)>,
    ) -> impl Fairing {
        let mut routes = Vec::new();

        routes.push(Route::new(
            Method::Get,
            callback_uri,
            redirect_handler::<A, C>,
        ));

        let mut default_scopes = vec![];
        if let Some((login_uri, login_scopes)) = login {
            routes.push(Route::new(Method::Get, login_uri, login_handler::<A, C>));
            default_scopes = login_scopes;
        }

        let oauth2 = Self {
            adapter,
            callback,
            config,
            default_scopes,
        };

        AdHoc::on_attach("OAuth Mount", |rocket| {
            Ok(rocket.manage(oauth2).mount("/", routes))
        })
    }

    /// Prepare an authentication redirect. This sets a state cookie and returns
    /// a `Redirect` to the provider's authorization page.
    pub fn get_redirect(
        &self,
        cookies: &mut Cookies<'_>,
        scopes: &[&str],
    ) -> Result<Redirect, A::Error> {
        let (uri, state) = self.adapter.authorization_uri(&self.config, scopes)?;
        cookies.add_private(
            Cookie::build(STATE_COOKIE_NAME, state.clone())
                .same_site(SameSite::Lax)
                .finish(),
        );
        Ok(Redirect::to(uri))
    }

    // TODO: Decide if BadRequest is the appropriate error code.
    // TODO: What do providers do if they *reject* the authorization?
    /// Handle the redirect callback, delegating to the adapter and callback to
    /// perform the token exchange and application-specific actions.
    fn handle<'r>(&self, request: &'r Request<'_>, _data: Data) -> handler::Outcome<'r> {
        // Parse the query data.
        let query = request.uri().query().into_outcome(Status::BadRequest)?;

        #[derive(FromForm)]
        struct CallbackQuery {
            code: String,
            state: String,
        }

        let params = match CallbackQuery::from_form(&mut FormItems::from(query), false) {
            Ok(p) => p,
            Err(_) => return handler::Outcome::failure(Status::BadRequest),
        };

        {
            // Verify that the given state is the same one in the cookie.
            // Begin a new scope so that cookies is not kept around too long.
            let mut cookies = request.guard::<Cookies<'_>>().expect("request cookies");
            match cookies.get_private(STATE_COOKIE_NAME) {
                Some(ref cookie) if cookie.value() == params.state => {
                    cookies.remove(cookie.clone());
                }
                _ => return handler::Outcome::failure(Status::BadRequest),
            }
        }

        // Have the adapter perform the token exchange.
        let token = match self.adapter.exchange_code(&self.config, &params.code) {
            Ok(token) => token,
            Err(e) => {
                log::error!("Token exchange failed: {:?}", e);
                return handler::Outcome::failure(Status::BadRequest);
            }
        };

        // Run the callback.
        let responder = self.callback.callback(request, token);
        handler::Outcome::from(request, responder)
    }
}

// These cannot be closures becuase of the lifetime parameter.
// TODO: cross-reference rust-lang/rust issues.

/// Handles the OAuth redirect route
fn redirect_handler<'r, A: Adapter, C: Callback>(
    request: &'r Request<'_>,
    data: Data,
) -> handler::Outcome<'r> {
    let oauth = match request.guard::<State<'_, OAuth2<A, C>>>() {
        Outcome::Success(oauth) => oauth,
        Outcome::Failure(_) => return handler::Outcome::failure(Status::InternalServerError),
        Outcome::Forward(()) => unreachable!(),
    };
    oauth.handle(request, data)
}

/// Handles a login route, performing a redirect
fn login_handler<'r, A: Adapter, C: Callback>(
    request: &'r Request<'_>,
    _data: Data,
) -> handler::Outcome<'r> {
    let oauth = match request.guard::<State<'_, OAuth2<A, C>>>() {
        Outcome::Success(oauth) => oauth,
        Outcome::Failure(_) => return handler::Outcome::failure(Status::InternalServerError),
        Outcome::Forward(()) => unreachable!(),
    };
    let mut cookies = request.guard::<Cookies<'_>>().expect("request cookies");
    let scopes: Vec<_> = oauth.default_scopes.iter().map(String::as_str).collect();
    handler::Outcome::from(request, oauth.get_redirect(&mut cookies, &scopes))
}

/// A `Provider` contains the authorization and token exchange URIs specific to
/// an OAuth service provider.
pub struct Provider {
    /// The authorization URI associated with the service provider.
    pub auth_uri: Cow<'static, str>,
    /// The token exchange URI associated with the service provider.
    pub token_uri: Cow<'static, str>,
}

impl Provider {
    fn from_config_value(conf: &Value) -> Result<Provider, ConfigError> {
        let type_error = || {
            ConfigError::BadType(
                "provider".into(),
                "known provider or table",
                "",
                PathBuf::new(),
            )
        };

        match conf {
            Value::String(s) => Provider::from_known_name(s).ok_or_else(type_error),
            Value::Table(t) => {
                let auth_uri = get_config_string(t, "auth_uri")?.into();
                let token_uri = get_config_string(t, "token_uri")?.into();

                Ok(Provider {
                    auth_uri,
                    token_uri,
                })
            }
            _ => Err(type_error()),
        }
    }
}

macro_rules! providers {
    (@ $(($name:ident $docstr:expr) : $auth:expr, $token:expr),*) => {
        $(
            #[doc = $docstr]
            #[allow(non_upper_case_globals)]
            pub const $name: Provider = Provider {
                auth_uri: Cow::Borrowed($auth),
                token_uri: Cow::Borrowed($token),
            };
        )*

        impl Provider {
            fn from_known_name(name: &str) -> Option<Provider> {
                match name {
                    $(
                        stringify!($name) => Some($name),
                    )*
                    _ => None,
                }
            }
        }
    };
    ($($name:ident : $auth:expr, $token:expr),* $(,)*) => {
        providers!(@ $(($name concat!("A `Provider` suitable for authorizing users with ", stringify!($name), ".")) : $auth, $token),*);
    };
}

providers! {
    Discord: "https://discordapp.com/api/oauth2/authorize", "https://discordapp.com/api/oauth2/token",
    Facebook: "https://www.facebook.com/v3.1/dialog/oauth", "https://graph.facebook.com/v3.1/oauth/access_token",
    GitHub: "https://github.com/login/oauth/authorize", "https://github.com/login/oauth/access_token",
    Google: "https://accounts.google.com/o/oauth2/v2/auth", "https://www.googleapis.com/oauth2/v4/token",
    Reddit: "https://www.reddit.com/api/v1/authorize", "https://www.reddit.com/api/v1/access_token",
    Yahoo: "https://api.login.yahoo.com/oauth2/request_auth", "https://api.login.yahoo.com/oauth2/get_token",
}
