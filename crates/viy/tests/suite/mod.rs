// cfg(test) so its helpers may unwrap, as test code (clippy.toml).
#[cfg(test)]
mod chat_pty;
#[cfg(test)]
mod completion;
#[cfg(test)]
mod live_images;
#[cfg(test)]
mod nebius_login;
#[cfg(test)]
mod resume_pty;
#[cfg(test)]
mod shell_op;
#[cfg(test)]
mod ui_pty;
mod version;
