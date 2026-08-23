use core::str;

use heapless::{String, Vec};

use crate::config::{DeviceConfig, MAX_PASSWORD_LEN, MAX_SSID_LEN, MAX_TOKEN_LEN};

#[derive(Debug, Eq, PartialEq)]
pub enum Request {
    ShowForm,
    Save(DeviceConfig),
    NotFound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestError {
    InvalidRequest,
    UnsupportedTransferEncoding,
    MissingContentLength,
    InvalidContentLength,
    UnsupportedContentType,
    RequestTooLarge,
    InvalidForm,
    InvalidEncoding,
    InvalidUtf8,
    InvalidConfig,
}

pub fn parse_request(bytes: &[u8], capacity: usize) -> Result<Option<Request>, RequestError> {
    let Some(header_end) = find_bytes(bytes, b"\r\n\r\n").map(|index| index + 4) else {
        if bytes.len() == capacity {
            return Err(RequestError::RequestTooLarge);
        }
        return Ok(None);
    };

    let headers = str::from_utf8(&bytes[..header_end]).map_err(|_| RequestError::InvalidUtf8)?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().ok_or(RequestError::InvalidRequest)?;

    if request_line == "GET / HTTP/1.1" || request_line == "GET / HTTP/1.0" {
        return Ok(Some(Request::ShowForm));
    }
    if request_line.starts_with("GET ") {
        return Ok(Some(Request::NotFound));
    }
    if request_line != "POST /save HTTP/1.1" && request_line != "POST /save HTTP/1.0" {
        return Ok(Some(Request::NotFound));
    }

    let mut content_length = None;
    let mut valid_content_type = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(RequestError::InvalidRequest);
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(RequestError::InvalidContentLength);
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| RequestError::InvalidContentLength)?,
            );
        } else if name.eq_ignore_ascii_case("content-type") {
            valid_content_type = value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim() == "application/x-www-form-urlencoded");
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(RequestError::UnsupportedTransferEncoding);
        }
    }

    let body_length = content_length.ok_or(RequestError::MissingContentLength)?;
    let total_length = header_end
        .checked_add(body_length)
        .ok_or(RequestError::RequestTooLarge)?;
    if total_length > capacity {
        return Err(RequestError::RequestTooLarge);
    }
    if bytes.len() < total_length {
        return Ok(None);
    }
    if !valid_content_type {
        return Err(RequestError::UnsupportedContentType);
    }

    parse_form(&bytes[header_end..total_length]).map(|request| Some(Request::Save(request)))
}

fn parse_form(body: &[u8]) -> Result<DeviceConfig, RequestError> {
    let body = str::from_utf8(body).map_err(|_| RequestError::InvalidUtf8)?;
    let mut ssid: Option<String<MAX_SSID_LEN>> = None;
    let mut password: Option<String<MAX_PASSWORD_LEN>> = None;
    let mut token: Option<String<MAX_TOKEN_LEN>> = None;

    for field in body.split('&') {
        let (name, value) = field.split_once('=').ok_or(RequestError::InvalidForm)?;
        match name {
            "ssid" if ssid.is_none() => ssid = Some(decode_component(value)?),
            "password" if password.is_none() => password = Some(decode_component(value)?),
            "token" if token.is_none() => token = Some(decode_component(value)?),
            _ => return Err(RequestError::InvalidForm),
        }
    }

    let ssid = ssid.ok_or(RequestError::InvalidForm)?;
    let password = password.ok_or(RequestError::InvalidForm)?;
    let token = token.ok_or(RequestError::InvalidForm)?;
    DeviceConfig::new(&ssid, &password, &token).map_err(|_| RequestError::InvalidConfig)
}

fn decode_component<const N: usize>(encoded: &str) -> Result<String<N>, RequestError> {
    let bytes = encoded.as_bytes();
    let mut decoded: Vec<u8, N> = Vec::new();
    let mut index = 0_usize;

    while index < bytes.len() {
        let byte = match bytes[index] {
            b'+' => b' ',
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(RequestError::InvalidEncoding);
                }
                let high = hex_value(bytes[index + 1]).ok_or(RequestError::InvalidEncoding)?;
                let low = hex_value(bytes[index + 2]).ok_or(RequestError::InvalidEncoding)?;
                index += 2;
                (high << 4) | low
            }
            byte => byte,
        };
        if byte == 0 {
            return Err(RequestError::InvalidEncoding);
        }
        decoded
            .push(byte)
            .map_err(|_| RequestError::InvalidForm)?;
        index += 1;
    }

    let value = str::from_utf8(&decoded).map_err(|_| RequestError::InvalidUtf8)?;
    let mut result = String::new();
    result
        .push_str(value)
        .map_err(|_| RequestError::InvalidForm)?;
    Ok(result)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
