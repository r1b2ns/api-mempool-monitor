use std::sync::Arc;

use a2::{
    Client, Endpoint, LocalizedNotificationBuilder, NotificationBuilder, NotificationOptions,
    PlainNotificationBuilder,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApnsError {
    /// Erros retornados pelo crate `a2` (rede, assinatura, resposta de erro do APNS, etc.)
    #[error("APNS error: {0}")]
    Client(#[from] a2::Error),
}

/// Wrapper em torno do cliente `a2::Client`.
///
/// Usa `Arc` internamente para ser barato de clonar e seguro de compartilhar
/// entre threads sem precisar de `Mutex`.
#[derive(Clone)]
pub struct ApnsClient {
    inner: Arc<Client>,
    /// Bundle ID do app iOS (ex.: "com.example.myapp")
    pub bundle_id: String,
    /// `true` = endpoint de produção; `false` = sandbox
    pub production: bool,
}

impl ApnsClient {
    /// Tenta construir um `ApnsClient` a partir de variáveis de ambiente.
    ///
    /// ### Variáveis obrigatórias
    /// | Variável          | Descrição                                          |
    /// |-------------------|----------------------------------------------------|
    /// | `APNS_KEY_ID`     | Key ID do portal Apple Developer                   |
    /// | `APNS_TEAM_ID`    | Team/Organization ID                               |
    /// | `APNS_BUNDLE_ID`  | Bundle ID do app (ex.: com.example.myapp)          |
    /// | `APNS_PRIVATE_KEY`| Conteúdo PEM da chave `.p8` (texto, não caminho)   |
    ///
    /// ### Variável opcional
    /// | Variável          | Padrão   | Descrição                         |
    /// |-------------------|----------|-----------------------------------|
    /// | `APNS_PRODUCTION` | `false`  | `true` para usar produção         |
    ///
    /// Retorna `Ok(None)` se alguma variável obrigatória não estiver definida.
    pub fn from_env() -> Result<Option<Self>, ApnsError> {
        let (key_id, team_id, bundle_id) = match (
            std::env::var("APNS_KEY_ID").ok(),
            std::env::var("APNS_TEAM_ID").ok(),
            std::env::var("APNS_BUNDLE_ID").ok(),
        ) {
            (Some(k), Some(t), Some(b)) => (k, t, b),
            _ => return Ok(None),
        };

        let production = std::env::var("APNS_PRODUCTION")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let endpoint = if production {
            Endpoint::Production
        } else {
            Endpoint::Sandbox
        };

        let pem = match std::env::var("APNS_PRIVATE_KEY") {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };

        let mut cursor = std::io::Cursor::new(pem.into_bytes());
        let inner = Client::token(&mut cursor, &key_id, &team_id, endpoint)?;

        Ok(Some(Self {
            inner: Arc::new(inner),
            bundle_id,
            production,
        }))
    }

    /// Envia uma notificação de alerta via APNS.
    ///
    /// - Se `title` for `Some`, usa `LocalizedNotificationBuilder` (suporta título + corpo).
    /// - Se apenas `body` for fornecido, usa `PlainNotificationBuilder`.
    /// - `custom_data` é serializado em um campo `"data"` no payload raiz.
    ///
    /// Retorna o `apns-id` atribuído pela Apple em caso de sucesso.
    pub async fn send_notification(
        &self,
        device_token: &str,
        title: Option<&str>,
        body: Option<&str>,
        badge: Option<u32>,
        sound: Option<&str>,
        custom_data: Option<&serde_json::Value>,
    ) -> Result<String, ApnsError> {
        let body_text = body.unwrap_or("");

        let options = NotificationOptions {
            apns_topic: Some(self.bundle_id.as_str()),
            ..Default::default()
        };

        let mut payload = if let Some(t) = title {
            let mut b = LocalizedNotificationBuilder::new(t, body_text);
            if let Some(n) = badge {
                b.set_badge(n);
            }
            if let Some(s) = sound {
                b.set_sound(s);
            }
            b.build(device_token, options)
        } else {
            let mut b = PlainNotificationBuilder::new(body_text);
            if let Some(n) = badge {
                b.set_badge(n);
            }
            if let Some(s) = sound {
                b.set_sound(s);
            }
            b.build(device_token, options)
        };

        if let Some(data) = custom_data {
            // ignora erros de serialização — Value sempre serializa corretamente
            let _ = payload.add_custom_data("data", data);
        }

        let response = self.inner.send(payload).await?;
        Ok(response.apns_id.unwrap_or_default())
    }
}
