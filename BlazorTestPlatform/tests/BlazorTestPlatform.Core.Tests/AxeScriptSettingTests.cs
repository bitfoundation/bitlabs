using BlazorTestPlatform.Configuration;

namespace BlazorTestPlatform.Core.Tests;

public sealed class AxeScriptSettingTests
{
    private static string Directory() => System.IO.Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "btp-axe-script", Guid.NewGuid().ToString("N"))).FullName;

    [Fact]
    public void The_bundled_engine_runs_unless_a_script_is_set()
    {
        Assert.Null(BtpSettings.Load(Directory(), new Dictionary<string, string?>()).Accessibility.AxeScriptPath);
    }

    [Fact]
    public void A_script_from_btp_json_resolves_against_the_project_and_the_environment_wins()
    {
        var directory = Directory();
        File.WriteAllText(Path.Combine(directory, "btp.json"), """{ "accessibility": { "axeScript": "tools/axe.min.js" } }""");

        Assert.Equal(Path.Combine(directory, "tools", "axe.min.js"), BtpSettings.Load(directory, new Dictionary<string, string?>()).Accessibility.AxeScriptPath);
        Assert.Equal(Path.Combine(directory, "other.js"), BtpSettings.Load(directory, new Dictionary<string, string?> { ["BTP_AXE_SCRIPT"] = "other.js" }).Accessibility.AxeScriptPath);
    }
}
