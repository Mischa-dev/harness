//! Fills in a prompt template such as `prompts/teammate-charter.md`. A
//! template names its values as `{{name}}`; every one must be given, so no
//! placeholder ever reaches a model. A leading `<!-- … -->` comment is for
//! people editing the template and is dropped.

/// The values a charter is filled in with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharterValues {
    pub agent_name: String,
    pub owner_name: String,
    /// Standing responsibilities, one each.
    pub responsibilities: Vec<String>,
}

impl CharterValues {
    pub fn render(&self, template: &str) -> Result<String, String> {
        let responsibilities = if self.responsibilities.is_empty() {
            format!("None yet. Ask {} what you should own.", self.owner_name)
        } else {
            self.responsibilities
                .iter()
                .map(|r| format!("- {}", r.trim()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        render(
            template,
            &[
                ("agent_name", self.agent_name.as_str()),
                ("owner_name", self.owner_name.as_str()),
                ("responsibilities", responsibilities.as_str()),
            ],
        )
    }
}

/// `template` with each `{{name}}` replaced by its value. A name with no
/// value is an error that names it.
pub fn render(template: &str, values: &[(&str, &str)]) -> Result<String, String> {
    let mut rest = template.trim_start();
    if rest.starts_with("<!--") {
        let end = rest
            .find("-->")
            .ok_or("the template's opening comment never closes")?;
        rest = rest[end + 3..].trim_start();
    }
    let mut out = String::with_capacity(rest.len());
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or("a {{ in the template never closes")?;
        let name = after[..end].trim();
        let value = values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
            .ok_or_else(|| format!("the template uses {{{{{name}}}}}, which has no value"))?;
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> CharterValues {
        CharterValues {
            agent_name: "Mara".into(),
            owner_name: "Mischa".into(),
            responsibilities: vec![
                "keep CI green".into(),
                " make the docs match the code ".into(),
            ],
        }
    }

    #[test]
    fn the_charter_is_filled_in_whole() {
        let charter = include_str!("../../../prompts/teammate-charter.md");
        let shown = values().render(charter).unwrap();
        assert!(shown.starts_with("# You are Mara"), "{}", &shown[..40]);
        assert!(!shown.contains("{{"));
        assert!(!shown.contains("<!--"));
        assert!(shown.contains("You work for Mischa"));
        assert!(shown.contains("- keep CI green\n- make the docs match the code\n"));
    }

    #[test]
    fn no_responsibilities_says_so() {
        let empty = CharterValues {
            responsibilities: vec![],
            ..values()
        };
        assert_eq!(
            empty.render("{{responsibilities}}").unwrap(),
            "None yet. Ask Mischa what you should own."
        );
    }

    #[test]
    fn every_placeholder_needs_a_value() {
        assert_eq!(
            render("Hi {{ who }}!", &[("who", "Mara")]).unwrap(),
            "Hi Mara!"
        );
        let err = render("Hi {{whom}}", &[("who", "Mara")]).unwrap_err();
        assert!(err.contains("{{whom}}"), "{err}");
        assert!(render("Hi {{who", &[("who", "Mara")]).is_err());
        assert!(render("<!-- open", &[]).is_err());
        // Values are inserted as they are, never read as templates.
        assert_eq!(render("{{a}}", &[("a", "{{b}}")]).unwrap(), "{{b}}");
    }
}
