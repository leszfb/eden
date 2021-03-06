/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Context;
use gotham::{
    handler::HandlerError,
    helpers::http::response::create_response,
    state::{request_id, State},
};
use gotham_ext::{error::HttpError, response::TryIntoResponse};
use hyper::{Body, Response};
use itertools::Itertools;
use std::iter;

use lfs_protocol::{git_lfs_mime, ResponseError};

use crate::errors::ErrorKind;
use crate::middleware::RequestContext;

pub fn build_response<IR>(
    res: Result<IR, HttpError>,
    mut state: State,
) -> Result<(State, Response<Body>), (State, HandlerError)>
where
    IR: TryIntoResponse,
{
    let res = res.and_then(|c| {
        c.try_into_response(&mut state)
            .context(ErrorKind::ResponseCreationFailure)
            .map_err(HttpError::e500)
    });

    match res {
        Ok(resp) => Ok((state, resp)),
        Err(error) => http_error_to_handler_error(error, state),
    }
}

pub fn http_error_to_handler_error(
    error: HttpError,
    mut state: State,
) -> Result<(State, Response<Body>), (State, HandlerError)> {
    let HttpError { error, status_code } = error;

    let error_message = iter::once(error.to_string())
        .chain(error.chain().skip(1).map(|c| c.to_string()))
        .join(": ");

    let res = ResponseError {
        message: error_message.clone(),
        documentation_url: None,
        request_id: Some(request_id(&state).to_string()),
    };

    if let Some(log_ctx) = state.try_borrow_mut::<RequestContext>() {
        log_ctx.set_error_msg(error_message);
    }

    // Bail if we can't convert the response to json.
    match serde_json::to_string(&res) {
        Ok(res) => {
            let res = create_response(&state, status_code, git_lfs_mime(), res);
            Ok((state, res))
        }
        Err(error) => Err((state, error.into())),
    }
}
