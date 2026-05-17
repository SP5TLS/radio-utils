use alloc::vec::Vec;

/// A single element in a Morse code sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MorseElement {
    Dit,
    Dash,
    ElementGap, // 1 dot duration (between dit/dash within a character)
    LetterGap,  // 3 dot durations (between characters)
    WordGap,    // 7 dot durations (between words)
}

/// ITU Morse code table.
pub const MORSE_TABLE: &[(char, &str)] = &[
    ('A', ".-"),
    ('B', "-..."),
    ('C', "-.-."),
    ('D', "-.."),
    ('E', "."),
    ('F', "..-."),
    ('G', "--."),
    ('H', "...."),
    ('I', ".."),
    ('J', ".---"),
    ('K', "-.-"),
    ('L', ".-.."),
    ('M', "--"),
    ('N', "-."),
    ('O', "---"),
    ('P', ".--."),
    ('Q', "--.-"),
    ('R', ".-."),
    ('S', "..."),
    ('T', "-"),
    ('U', "..-"),
    ('V', "...-"),
    ('W', ".--"),
    ('X', "-..-"),
    ('Y', "-.--"),
    ('Z', "--.."),
    ('0', "-----"),
    ('1', ".----"),
    ('2', "..---"),
    ('3', "...--"),
    ('4', "....-"),
    ('5', "....."),
    ('6', "-...."),
    ('7', "--..."),
    ('8', "---.."),
    ('9', "----."),
    ('.', ".-.-.-"),
    (',', "--..--"),
    ('?', "..--.."),
    ('\'', ".----."),
    ('!', "-.-.--"),
    ('/', "-..-."),
    ('(', "-.--."),
    (')', "-.--.-"),
    ('&', ".-..."),
    (':', "---..."),
    (';', "-.-.-."),
    ('=', "-...-"),
    ('+', ".-.-."),
    ('-', "-....-"),
    ('_', "..--.-"),
    ('"', ".-..-."),
    ('$', "...-..-"),
    ('@', ".--.-."),
];

/// Look up the Morse pattern for a character (case-insensitive).
pub fn char_to_morse(c: char) -> Option<&'static str> {
    let upper = c.to_ascii_uppercase();
    MORSE_TABLE
        .iter()
        .find(|(ch, _)| *ch == upper)
        .map(|(_, m)| *m)
}

/// Convert a text string into a sequence of Morse elements.
///
/// - Each '.' in pattern becomes `Dit`, each '-' becomes `Dash`
/// - Between elements within a character: `ElementGap`
/// - Between characters: `LetterGap`
/// - Space character: `WordGap`
/// - Unknown characters are silently skipped
/// - No trailing gaps
pub fn text_to_elements(text: &str) -> Vec<MorseElement> {
    let mut elements = Vec::new();
    let mut first_char = true;

    for c in text.chars() {
        if c == ' ' {
            // Remove trailing element/letter gap before word gap
            while elements.last() == Some(&MorseElement::LetterGap)
                || elements.last() == Some(&MorseElement::ElementGap)
            {
                elements.pop();
            }
            elements.push(MorseElement::WordGap);
            first_char = true;
            continue;
        }

        if let Some(pattern) = char_to_morse(c) {
            if !first_char {
                // Letter gap between characters — remove trailing element gap first
                if elements.last() == Some(&MorseElement::ElementGap) {
                    elements.pop();
                }
                elements.push(MorseElement::LetterGap);
            }
            first_char = false;

            for (i, symbol) in pattern.chars().enumerate() {
                if i > 0 {
                    elements.push(MorseElement::ElementGap);
                }
                match symbol {
                    '.' => elements.push(MorseElement::Dit),
                    '-' => elements.push(MorseElement::Dash),
                    _ => {}
                }
            }
        }
        // Unknown chars are silently skipped
    }

    // Clean trailing gaps
    while matches!(
        elements.last(),
        Some(MorseElement::ElementGap | MorseElement::LetterGap)
    ) {
        elements.pop();
    }

    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_standard_characters() {
        assert_eq!(char_to_morse('A'), Some(".-"));
        assert_eq!(char_to_morse('B'), Some("-..."));
        assert_eq!(char_to_morse('0'), Some("-----"));
        assert_eq!(char_to_morse('9'), Some("----."));
        assert_eq!(char_to_morse('/'), Some("-..-."));
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(char_to_morse('a'), char_to_morse('A'));
        assert_eq!(char_to_morse('z'), char_to_morse('Z'));
    }

    #[test]
    fn unknown_char_returns_none() {
        assert_eq!(char_to_morse('€'), None);
        assert_eq!(char_to_morse('💀'), None);
    }

    #[test]
    fn text_to_elements_simple() {
        // H = ....  I = ..
        use MorseElement::*;
        let expected = vec![
            Dit, ElementGap, Dit, ElementGap, Dit, ElementGap, Dit, // H
            LetterGap, Dit, ElementGap, Dit, // I
        ];
        assert_eq!(text_to_elements("HI"), expected);
    }

    #[test]
    fn text_to_elements_with_space() {
        // A = .-   B = -...
        use MorseElement::*;
        let expected = vec![
            Dit, ElementGap, Dash, // A
            WordGap, Dash, ElementGap, Dit, ElementGap, Dit, ElementGap, Dit, // B
        ];
        assert_eq!(text_to_elements("A B"), expected);
    }

    #[test]
    fn unknown_chars_are_skipped() {
        assert_eq!(text_to_elements("A€B"), text_to_elements("AB"));
    }
}
