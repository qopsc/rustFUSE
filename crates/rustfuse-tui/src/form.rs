//! A small form: labelled text fields (some hidden, like tokens) and
//! fields that cycle through a few choices.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    /// Shown as dots.
    Secret,
    /// One of these; Left and Right (or Space) change it.
    Choice(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct Field {
    pub label: String,
    pub value: String,
    pub kind: FieldKind,
    /// Shown dimmed when the field is empty.
    pub hint: String,
    /// Cursor position in `value`, in chars.
    pub cursor: usize,
}

impl Field {
    pub fn text(label: &str, value: &str, hint: &str) -> Field {
        Field {
            label: label.into(),
            value: value.into(),
            kind: FieldKind::Text,
            hint: hint.into(),
            cursor: value.chars().count(),
        }
    }

    pub fn secret(label: &str, hint: &str) -> Field {
        Field {
            kind: FieldKind::Secret,
            ..Field::text(label, "", hint)
        }
    }

    pub fn choice(label: &str, options: &[&str], value: &str) -> Field {
        let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
        let value = if options.iter().any(|o| o == value) {
            value.to_string()
        } else {
            options[0].clone()
        };
        Field {
            kind: FieldKind::Choice(options),
            ..Field::text(label, &value, "")
        }
    }

    /// The text to draw for the value.
    pub fn shown(&self) -> String {
        match self.kind {
            FieldKind::Secret => "•".repeat(self.value.chars().count()),
            _ => self.value.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Form {
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    /// Shown under the fields.
    pub note: String,
    pub error: Option<String>,
}

/// What a key did to the form.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Submit,
    Cancel,
}

impl Form {
    pub fn new(title: &str, fields: Vec<Field>) -> Form {
        Form {
            title: title.into(),
            fields,
            focus: 0,
            note: String::new(),
            error: None,
        }
    }

    pub fn with_note(mut self, note: &str) -> Form {
        self.note = note.into();
        self
    }

    /// The value of the field labelled `label`, trimmed.
    pub fn get(&self, label: &str) -> String {
        self.field(label)
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default()
    }

    pub fn field(&self, label: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.label == label)
    }

    pub fn set(&mut self, label: &str, value: &str) {
        if let Some(f) = self.fields.iter_mut().find(|f| f.label == label) {
            f.value = value.to_string();
            f.cursor = value.chars().count();
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.fields.len();
        let field = &mut self.fields[self.focus];
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Enter if self.focus + 1 == n => return Outcome::Submit,
            KeyCode::Char('s') if ctrl => return Outcome::Submit,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.focus = (self.focus + n - 1) % n,
            _ => match &field.kind {
                FieldKind::Choice(options) => {
                    let i = options.iter().position(|o| *o == field.value).unwrap_or(0);
                    let next = match key.code {
                        KeyCode::Left => (i + options.len() - 1) % options.len(),
                        KeyCode::Right | KeyCode::Char(' ') => (i + 1) % options.len(),
                        _ => i,
                    };
                    field.value = options[next].clone();
                }
                _ => edit(field, key),
            },
        }
        self.error = None;
        Outcome::Continue
    }
}

fn edit(f: &mut Field, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut chars: Vec<char> = f.value.chars().collect();
    f.cursor = f.cursor.min(chars.len());
    match key.code {
        KeyCode::Char('u') if ctrl => {
            chars.drain(..f.cursor);
            f.cursor = 0;
        }
        KeyCode::Char('a') if ctrl => f.cursor = 0,
        KeyCode::Char('e') if ctrl => f.cursor = chars.len(),
        KeyCode::Char(c) if !ctrl => {
            chars.insert(f.cursor, c);
            f.cursor += 1;
        }
        KeyCode::Backspace if f.cursor > 0 => {
            f.cursor -= 1;
            chars.remove(f.cursor);
        }
        KeyCode::Delete if f.cursor < chars.len() => {
            chars.remove(f.cursor);
        }
        KeyCode::Left => f.cursor = f.cursor.saturating_sub(1),
        KeyCode::Right => f.cursor = (f.cursor + 1).min(chars.len()),
        KeyCode::Home => f.cursor = 0,
        KeyCode::End => f.cursor = chars.len(),
        _ => {}
    }
    f.value = chars.into_iter().collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(form: &mut Form, code: KeyCode) -> Outcome {
        form.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn typing_moving_and_choosing() {
        let mut form = Form::new(
            "t",
            vec![
                Field::text("Name", "", ""),
                Field::choice("Mode", &["folder", "mount"], "mount"),
            ],
        );
        for c in "prj".chars() {
            press(&mut form, KeyCode::Char(c));
        }
        press(&mut form, KeyCode::Left);
        press(&mut form, KeyCode::Char('o'));
        assert_eq!(form.get("Name"), "proj");
        assert_eq!(press(&mut form, KeyCode::Enter), Outcome::Continue);
        press(&mut form, KeyCode::Right);
        assert_eq!(form.get("Mode"), "folder");
        assert_eq!(press(&mut form, KeyCode::Enter), Outcome::Submit);
        assert_eq!(press(&mut form, KeyCode::Esc), Outcome::Cancel);
    }
}
