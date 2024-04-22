//@ run-rustfix

fn get_vowel_count(string: &str) -> usize {
    string
        .chars()
        .filter(|c| "aeiou".contains(c))
        //~^ ERROR the trait bound `&char: Pattern<'_>` is not satisfied
        .count()
}

fn main() {
    let _ = get_vowel_count("asdf");
}
