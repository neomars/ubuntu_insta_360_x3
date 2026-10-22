//! Erreurs typées : l'API HTTP traduit `MediaError` / `ExportError` en 400, le reste en 500.

use thiserror::Error;

/// Fichier illisible ou format non reconnu.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct MediaError(pub String);

/// Paramètres d'export invalides ou échec de FFmpeg / du rendu.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ExportError(pub String);

/// Tâche annulée par l'utilisateur.
#[derive(Debug, Error)]
#[error("annulé")]
pub struct Cancelled;

pub type Result<T> = anyhow::Result<T>;

pub fn media_err(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(MediaError(msg.into()))
}

pub fn export_err(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ExportError(msg.into()))
}

pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}

/// Vrai pour les erreurs « de l'utilisateur » (HTTP 400, message affiché tel quel).
pub fn is_user_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<MediaError>().is_some() || e.downcast_ref::<ExportError>().is_some()
}
