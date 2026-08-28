use hyper::StatusCode;
use serde::{Deserialize, Serialize};

pub(crate) const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub(crate) const MAX_QUEUE_RECORDS: usize = 500;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Point {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Size {
    pub(crate) width: f64,
    pub(crate) height: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PageContext {
    pub(crate) url: String,
    pub(crate) title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TargetContext {
    pub(crate) selector: String,
    pub(crate) tag: String,
    pub(crate) id: Option<String>,
    pub(crate) classes: Vec<String>,
    pub(crate) selected_text: Option<String>,
    pub(crate) text: String,
    pub(crate) html: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PointerContext {
    pub(crate) page: Point,
    pub(crate) viewport: Point,
    pub(crate) target: Point,
    pub(crate) scroll: Point,
    pub(crate) viewport_size: Size,
    pub(crate) target_size: Size,
    pub(crate) device_pixel_ratio: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CommentDraft {
    pub(crate) comment: String,
    pub(crate) page: PageContext,
    pub(crate) target: TargetContext,
    pub(crate) pointer: PointerContext,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommentRecord {
    pub(crate) version: u8,
    pub(crate) id: String,
    pub(crate) timestamp: String,
    pub(crate) comment: String,
    pub(crate) page: PageContext,
    pub(crate) target: TargetContext,
    pub(crate) pointer: PointerContext,
}

impl From<CommentDraft> for CommentRecord {
    fn from(draft: CommentDraft) -> Self {
        let CommentDraft {
            comment,
            page,
            target,
            pointer,
        } = draft;
        Self {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: jiff::Timestamp::now().to_string(),
            comment,
            page,
            target,
            pointer,
        }
    }
}

#[derive(Debug)]
pub(crate) struct RequestError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
    pub(crate) allow: Option<&'static str>,
}

impl RequestError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            allow: None,
        }
    }

    pub(crate) fn method_not_allowed(allow: &'static str) -> Self {
        Self {
            status: StatusCode::METHOD_NOT_ALLOWED,
            message: "method not allowed".to_owned(),
            allow: Some(allow),
        }
    }
}

impl CommentDraft {
    pub(crate) fn validate(mut self) -> Result<Self, RequestError> {
        self.comment = self.comment.trim().to_owned();
        require_nonempty(&self.comment, "comment")?;
        require_length(&self.comment, "comment", 10_000)?;
        require_length(&self.page.url, "page.url", 4_096)?;
        require_length(&self.page.title, "page.title", 512)?;
        require_length(&self.target.selector, "target.selector", 2_048)?;
        require_length(&self.target.tag, "target.tag", 64)?;
        require_length(&self.target.text, "target.text", 4_000)?;
        require_length(&self.target.html, "target.html", 8_000)?;

        if let Some(id) = &self.target.id {
            require_length(id, "target.id", 512)?;
        }
        if let Some(selected_text) = &self.target.selected_text {
            require_length(selected_text, "target.selectedText", 2_000)?;
        }
        if self.target.classes.len() > 64 {
            return Err(RequestError::new(
                StatusCode::BAD_REQUEST,
                "target.classes must contain at most 64 strings",
            ));
        }
        for (index, class_name) in self.target.classes.iter().enumerate() {
            require_length(class_name, &format!("target.classes[{index}]"), 256)?;
        }

        validate_point(&self.pointer.page, "pointer.page")?;
        validate_point(&self.pointer.viewport, "pointer.viewport")?;
        validate_point(&self.pointer.target, "pointer.target")?;
        validate_point(&self.pointer.scroll, "pointer.scroll")?;
        validate_size(&self.pointer.viewport_size, "pointer.viewportSize")?;
        validate_size(&self.pointer.target_size, "pointer.targetSize")?;
        let ratio = self.pointer.device_pixel_ratio;
        if !ratio.is_finite() || !(0.0 < ratio && ratio <= 100.0) {
            return Err(RequestError::new(
                StatusCode::BAD_REQUEST,
                "pointer.devicePixelRatio is outside the supported range",
            ));
        }

        Ok(self)
    }
}

fn require_nonempty(value: &str, label: &str) -> Result<(), RequestError> {
    if value.is_empty() {
        return Err(RequestError::new(
            StatusCode::BAD_REQUEST,
            format!("{label} must not be blank"),
        ));
    }
    Ok(())
}

fn require_length(value: &str, label: &str, maximum: usize) -> Result<(), RequestError> {
    if value.chars().count() > maximum {
        return Err(RequestError::new(
            StatusCode::BAD_REQUEST,
            format!("{label} must be at most {maximum} characters"),
        ));
    }
    Ok(())
}

fn validate_number(value: f64, label: &str) -> Result<(), RequestError> {
    if !value.is_finite() || value.abs() > 1_000_000_000.0 {
        return Err(RequestError::new(
            StatusCode::BAD_REQUEST,
            format!("{label} is outside the supported range"),
        ));
    }
    Ok(())
}

fn validate_point(point: &Point, label: &str) -> Result<(), RequestError> {
    validate_number(point.x, &format!("{label}.x"))?;
    validate_number(point.y, &format!("{label}.y"))
}

fn validate_size(size: &Size, label: &str) -> Result<(), RequestError> {
    validate_number(size.width, &format!("{label}.width"))?;
    validate_number(size.height, &format!("{label}.height"))?;
    if size.width < 0.0 || size.height < 0.0 {
        return Err(RequestError::new(
            StatusCode::BAD_REQUEST,
            format!("{label} must not be negative"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CommentDraft, PageContext, Point, PointerContext, Size, TargetContext};

    fn draft(comment: &str) -> CommentDraft {
        CommentDraft {
            comment: comment.to_owned(),
            page: PageContext {
                url: "http://localhost/".to_owned(),
                title: "Fixture".to_owned(),
            },
            target: TargetContext {
                selector: "#intro".to_owned(),
                tag: "p".to_owned(),
                id: Some("intro".to_owned()),
                classes: vec!["lead".to_owned()],
                selected_text: Some("selected words".to_owned()),
                text: "A paragraph".to_owned(),
                html: "<p id=\"intro\">A paragraph</p>".to_owned(),
            },
            pointer: PointerContext {
                page: Point { x: 10.0, y: 20.0 },
                viewport: Point { x: 10.0, y: 20.0 },
                target: Point { x: 2.0, y: 3.0 },
                scroll: Point { x: 0.0, y: 0.0 },
                viewport_size: Size {
                    width: 1_280.0,
                    height: 720.0,
                },
                target_size: Size {
                    width: 500.0,
                    height: 80.0,
                },
                device_pixel_ratio: 1.0,
            },
        }
    }

    #[test]
    fn trims_and_accepts_a_valid_draft() {
        let validated = draft("  Rewrite this.  ").validate().expect("valid draft");
        assert_eq!(validated.comment, "Rewrite this.");
    }

    #[test]
    fn rejects_blank_and_oversized_fields() {
        assert!(draft(" \n ").validate().is_err());
        let mut oversized = draft("ok");
        oversized.target.selected_text = Some("x".repeat(2_001));
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn rejects_invalid_geometry() {
        let mut invalid = draft("ok");
        invalid.pointer.target_size.width = -1.0;
        assert!(invalid.validate().is_err());
    }
}
