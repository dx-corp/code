use super::*;

pub(super) async fn write_sse_stream<S>(
    socket: &mut S,
    replay: Vec<StreamEnvelope>,
    mut rx: broadcast::Receiver<StreamEnvelope>,
    shared: Box<SharedRunner>,
    mut filter: Box<TranscriptStreamFilter>,
    controller_authorization: Option<ControllerStreamAuthorization>,
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_sse_headers(socket).await?;
    if controller_authorization
        .as_ref()
        .is_some_and(|authorization| !shared.controller_stream_is_authorized(authorization))
    {
        return close_sse_stream(socket).await;
    }
    for envelope in replay {
        for envelope in filter.apply(envelope) {
            if !write_sse_event_if_authorized(
                socket,
                &shared,
                controller_authorization.as_ref(),
                &envelope,
            )
            .await?
            {
                return close_sse_stream(socket).await;
            }
        }
    }
    loop {
        let next = tokio::select! {
            biased;
            () = async {
                match controller_authorization.as_ref() {
                    Some(authorization) => authorization.cancellation.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => break,
            next = rx.recv() => next,
        };
        match next {
            Ok(envelope) => {
                if controller_authorization
                    .as_ref()
                    .is_some_and(|authorization| {
                        !shared.controller_stream_is_authorized(authorization)
                    })
                {
                    break;
                }
                for envelope in filter.apply(envelope) {
                    if !write_sse_event_if_authorized(
                        socket,
                        &shared,
                        controller_authorization.as_ref(),
                        &envelope,
                    )
                    .await?
                    {
                        return close_sse_stream(socket).await;
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                let envelope = shared.reset_envelope(format!("broadcast_lag:{skipped}"));
                for envelope in filter.apply(envelope) {
                    if !write_sse_event_if_authorized(
                        socket,
                        &shared,
                        controller_authorization.as_ref(),
                        &envelope,
                    )
                    .await?
                    {
                        return close_sse_stream(socket).await;
                    }
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    close_sse_stream(socket).await
}
