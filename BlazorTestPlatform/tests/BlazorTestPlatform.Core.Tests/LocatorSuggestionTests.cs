namespace BlazorTestPlatform.Core.Tests;

public sealed class LocatorSuggestionTests
{
    private static readonly string[] Tree =
    [
        "- heading \"Publish changes?\" [level=2]",
        "- button \"Cancel\"",
        "- button \"Publish\"",
        "- link \"Publish settings\"",
        "- textbox \"Email address\"",
        "- status",
    ];

    private static IReadOnlyList<string> Suggest(By query) => LocatorSuggestions.For(Drivers.LocatorChain.Start(query), Tree);

    [Fact]
    public void A_misspelled_name_suggests_the_same_role_first()
    {
        Assert.Equal(["ctx.GetByRole(\"button\", \"Publish\")"], Suggest(By.Role("button", "Publsh")));
        Assert.Equal(["ctx.GetByRole(\"button\", \"Publish\")", "ctx.GetByRole(\"link\", \"Publish settings\")"], Suggest(By.Role("button", "Publish set")).Take(2));
    }

    [Fact]
    public void Text_and_label_queries_suggest_by_name_under_any_role()
    {
        Assert.Equal("ctx.GetByRole(\"textbox\", \"Email address\")", Suggest(By.Label("Email")).First());
        Assert.Equal("ctx.GetByRole(\"heading\", \"Publish changes?\")", Suggest(By.Text("Publish changes")).First());
    }

    [Fact]
    public void A_different_name_of_the_same_role_is_not_suggested()
    {
        Assert.Empty(Suggest(By.Role("button", "Save")));
    }

    [Fact]
    public void Names_with_quotes_are_read_and_written_back_escaped()
    {
        var suggestions = LocatorSuggestions.For(Drivers.LocatorChain.Start(By.Role("button", "Say hello")), ["- button \"Say \\\"hello\\\"\""]);

        Assert.Equal(["ctx.GetByRole(\"button\", \"Say \\\"hello\\\"\")"], suggestions);
    }

    [Fact]
    public void Nothing_close_suggests_nothing()
    {
        Assert.Empty(Suggest(By.Role("checkbox", "Remember me")));
        Assert.Empty(Suggest(By.Css(".btn")));
    }
}
