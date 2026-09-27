/// Handwritten mandatory properties for artifact templates that the generated
/// combat tables currently miss.
///
/// Warlock's Ring is shipped as a ring template, while `gamedata.rs` only exposes
/// mandatory properties from the generated weapon/armor/shield combat tables.
pub fn mandatory_properties(template_uuid: &str) -> &'static [(&'static str, u8)] {
    match template_uuid {
        "23607f09-a103-4ed3-a0de-33e0498f8018" => &[
            ("874f28f1-cdc3-4af5-af4e-4e4d0e2a4969", 7),
            ("8a107a03-26ed-4ea9-97ac-e7f44c2bb5d3", 5),
            ("18ef65f2-7585-401b-b7a4-3fe66a830721", 5),
            ("7ea448a6-0415-461f-8ef8-2cec9f5004f9", 5),
            ("5665da58-5fce-4b5e-863d-0395afdf3554", 5),
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::mandatory_properties;

    const SOURCE: &str = include_str!("../../../data/artifact_mandatory_properties_source.json");

    #[test]
    fn handwritten_artifact_properties_match_committed_source() {
        let source: BTreeMap<String, Vec<(String, u8)>> = serde_json::from_str(SOURCE).unwrap();
        assert_eq!(source.len(), 1);
        for (template_uuid, expected) in source {
            let got: Vec<_> = mandatory_properties(&template_uuid)
                .iter()
                .map(|(property_uuid, tier)| ((*property_uuid).to_string(), *tier))
                .collect();
            assert_eq!(got, expected, "{template_uuid}");
        }
    }

    #[test]
    fn unknown_template_has_no_handwritten_properties() {
        assert!(mandatory_properties("09aa3390-8f42-4cd5-a88c-5c94d5e1dd29").is_empty());
    }
}
