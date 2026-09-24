//! The work behind the account form's Connect button, off the UI thread: the
//! Google sign-in when the account uses one, the login test of both servers,
//! and saving. Each step reports through the main window's `Proxy`.

use esmail::auth::Auth;
use esmail::config::{AccountConfig, AuthKind, Config};
use esmail_win32::core_glue::account_form::AccountForm;
use esmail_win32::core_glue::account_setup::{google_auth, google_client_or_explain, remove_account, save_account, test_connection};
use tokio::runtime::{Builder, Runtime};
use tokio::task::AbortHandle;
use win32ui::prelude::*;

use super::Outcome;
use crate::app::{App, Msg};

/// The runtime the connection work runs on, and the task in flight.
#[derive(Default)]
pub struct Worker {
    runtime: Option<Runtime>,
    task: Option<AbortHandle>,
}

impl Worker {
    fn runtime(&mut self) -> std::io::Result<&Runtime> {
        if self.runtime.is_none() {
            self.runtime = Some(Builder::new_multi_thread().worker_threads(1).thread_name("esmail-accounts").enable_all().build()?);
        }
        Ok(self.runtime.as_ref().expect("just created"))
    }

    /// Stops the work in flight: a Google sign-in stops waiting (which closes
    /// its redirect listener), a connection test is dropped.
    pub fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    /// The work in flight has reported its end.
    pub fn finished(&mut self) {
        self.task = None;
    }
}

fn report(proxy: &Proxy<Msg>, outcome: Outcome) {
    let _ = proxy.send(Msg::AccountOutcome(outcome));
}

/// The credentials `account` signs in with, from the form's password or Google.
async fn credentials(config: &Config, account: &AccountConfig, form: &AccountForm, sign_in_again: bool, proxy: &Proxy<Msg>) -> std::result::Result<Auth, String> {
    match account.auth {
        AuthKind::Password => Ok(Auth::password(form.password.clone())),
        AuthKind::GoogleOAuth => {
            let client = google_client_or_explain(config)?;
            let awaiting = || report(proxy, Outcome::AwaitingBrowser);
            let unavailable = |url| report(proxy, Outcome::BrowserUnavailable(url));
            let auth = google_auth(&client, account, sign_in_again, awaiting, unavailable).await?;
            report(proxy, Outcome::Working("Signed in with Google. Checking the connection...".to_string()));
            Ok(auth)
        }
    }
}

impl App {
    /// The form's Connect: sign in if needed, test both servers, save, and
    /// report. Nothing is saved unless every step worked.
    pub(in crate::app) fn connect(&mut self, ui: &Ui<Msg>, form: AccountForm, editing: Option<String>, sign_in_again: bool) {
        let id = editing.unwrap_or_else(|| form.id());
        let base = self.config.accounts.iter().find(|account| account.id == id);
        let account = match form.to_account(base) {
            Ok(account) => account,
            Err(problem) => return self.account_outcome(ui, Outcome::Failed(problem.message.to_string())),
        };
        let runtime = match self.accounts.connect.runtime() {
            Ok(runtime) => runtime,
            Err(error) => return self.account_outcome(ui, Outcome::Failed(format!("Could not start the connection: {error}"))),
        };
        let config = self.config.clone();
        let proxy = ui.proxy();
        let task = runtime.spawn(async move {
            let auth = match credentials(&config, &account, &form, sign_in_again, &proxy).await {
                Ok(auth) => auth,
                Err(error) => return report(&proxy, Outcome::Failed(error)),
            };
            if let Err(error) = test_connection(&account, &auth).await {
                return report(&proxy, Outcome::Failed(error));
            }
            report(&proxy, Outcome::Working("Saving...".to_string()));
            let id = account.id.clone();
            let saved = tokio::task::spawn_blocking(move || save_account(&config, account, &auth)).await;
            report(&proxy, match saved {
                Ok(Ok(config)) => Outcome::Saved { config, id },
                Ok(Err(error)) => Outcome::Failed(error),
                Err(error) => Outcome::Failed(format!("Saving failed: {error}")),
            });
        });
        self.accounts.connect.task = Some(task.abort_handle());
    }
}

/// Forgets the account `id` on a thread of its own (the keyring and the file
/// system are not the UI thread's to wait for).
pub fn remove(ui: &Ui<Msg>, config: Config, id: String) {
    let proxy = ui.proxy();
    std::thread::spawn(move || {
        let outcome = match remove_account(&config, &id) {
            Ok(config) => Outcome::Removed { config, id },
            Err(error) => Outcome::Failed(error),
        };
        report(&proxy, outcome);
    });
}
