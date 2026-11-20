use super::*;

#[test]
fn one_groupby_with_a_count_is_read() {
    for text in [
        "groupby((kind),aggregate($count as count))",
        " groupby( ( kind ) , aggregate( $count  as  count ) ) ",
    ] {
        assert_eq!(
            parse_apply(text).expect(text),
            GroupCount {
                property: "kind".to_string(),
                alias: "count".to_string(),
            }
        );
    }
    assert_eq!(
        parse_apply("groupby((Status),aggregate($count as n))")
            .expect("alias")
            .alias,
        "n"
    );
}

#[test]
fn anything_else_is_refused() {
    for text in [
        "",
        "groupby((kind))",
        "groupby((kind,title),aggregate($count as count))",
        "groupby((a b),aggregate($count as count))",
        "groupby((kind),aggregate(up with sum as total))",
        "groupby((kind),aggregate($count as count))/filter(count gt 1)",
        "filter(kind eq 'a')/groupby((kind),aggregate($count as count))",
        "aggregate($count as count)",
        "groupby((kind),aggregate($count as count, $count as n))",
        "groupby((kind),aggregate($count))",
        "groupby((fields/kind),aggregate($count as count))",
    ] {
        assert!(parse_apply(text).is_err(), "{text} is refused");
    }
}
