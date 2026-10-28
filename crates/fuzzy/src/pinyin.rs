use pinyin::ToPinyinMulti;

const MAX_INITIAL_VARIANTS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinyinInitials {
    variants: Vec<PinyinInitialVariant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PinyinInitialVariant {
    text: String,
    byte_offsets: Vec<usize>,
}

impl PinyinInitials {
    pub fn from_text(text: &str) -> Option<Self> {
        let mut variants = vec![PinyinInitialVariant {
            text: String::new(),
            byte_offsets: Vec::new(),
        }];
        let mut has_pinyin = false;

        for (byte_offset, character) in text.char_indices() {
            let mut initials = Vec::new();
            if let Some(pronunciations) = character.to_pinyin_multi() {
                for pronunciation in pronunciations {
                    let Some(initial) = pronunciation.plain().as_bytes().first().copied() else {
                        continue;
                    };
                    let initial = initial.to_ascii_lowercase();
                    if !initials.contains(&initial) {
                        initials.push(initial);
                    }
                }
                has_pinyin = true;
            }

            if initials.is_empty() {
                for variant in &mut variants {
                    let character = character.to_ascii_lowercase();
                    variant.text.push(character);
                    variant
                        .byte_offsets
                        .extend(std::iter::repeat_n(byte_offset, character.len_utf8()));
                }
                continue;
            }

            let previous_variants = variants;
            variants = Vec::with_capacity(
                (previous_variants.len() * initials.len()).min(MAX_INITIAL_VARIANTS),
            );
            'variants: for previous in previous_variants {
                for initial in &initials {
                    let mut variant = previous.clone();
                    variant.text.push(char::from(*initial));
                    variant.byte_offsets.push(byte_offset);
                    variants.push(variant);
                    if variants.len() == MAX_INITIAL_VARIANTS {
                        break 'variants;
                    }
                }
            }
        }

        has_pinyin.then_some(Self { variants })
    }

    pub fn variants(&self) -> impl Iterator<Item = (&str, &[usize])> {
        self.variants
            .iter()
            .map(|variant| (variant.text.as_str(), variant.byte_offsets.as_slice()))
    }

    pub fn original_positions(byte_offsets: &[usize], positions: &[usize]) -> Vec<usize> {
        let mut original_positions = positions
            .iter()
            .filter_map(|position| byte_offsets.get(*position).copied())
            .collect::<Vec<_>>();
        original_positions.sort_unstable();
        original_positions.dedup();
        original_positions
    }
}

#[cfg(test)]
mod tests {
    use super::PinyinInitials;

    #[test]
    fn builds_initials_for_chinese_and_preserves_other_characters() {
        let initials = PinyinInitials::from_text("编辑器: 转到定义").expect("contains Chinese");
        let (text, offsets) = initials.variants().next().expect("has variant");
        assert_eq!(text, "bjq: zddy");
        assert_eq!(
            PinyinInitials::original_positions(offsets, &[0, 1, 2]),
            vec![0, 3, 6]
        );
    }

    #[test]
    fn produces_heteronym_variants() {
        let initials = PinyinInitials::from_text("重庆").expect("contains Chinese");
        let variants = initials
            .variants()
            .map(|(text, _)| text)
            .collect::<Vec<_>>();
        assert!(variants.contains(&"zq"));
        assert!(variants.contains(&"cq"));
    }

    #[test]
    fn skips_strings_without_pinyin() {
        assert_eq!(PinyinInitials::from_text("editor: save"), None);
    }
}
