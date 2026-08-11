use codex_cli_protocol::SkillMetadata;

use crate::skills_helpers::skill_description;
use crate::skills_helpers::skill_display_name;

use super::candidate::Candidate;
use super::candidate::MentionType;
use super::candidate::Selection;

pub(crate) fn build_search_catalog(skills: Option<&[SkillMetadata]>) -> Vec<Candidate> {
    skills.into_iter().flatten().map(skill_candidate).collect()
}

fn skill_candidate(skill: &SkillMetadata) -> Candidate {
    let display_name = skill_display_name(skill);
    let description = Some(skill_description(skill).to_string());
    let skill_name = skill.name.clone();
    let search_terms = if display_name == skill.name {
        vec![skill_name.clone()]
    } else {
        vec![skill_name.clone(), display_name.clone()]
    };
    Candidate {
        display_name,
        description,
        search_terms,
        mention_type: MentionType::Skill,
        selection: Selection::Tool {
            insert_text: format!("${skill_name}"),
            path: Some(skill.path.to_string_lossy().into_owned()),
        },
    }
}
