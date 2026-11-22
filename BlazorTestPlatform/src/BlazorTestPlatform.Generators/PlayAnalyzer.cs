using System.Collections.Immutable;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Diagnostics;
using Microsoft.CodeAnalysis.Operations;

namespace BlazorTestPlatform.Generators;

/// <summary>
/// BTP006: an assertion, action or harness call whose task is dropped. The play script moves on before the check runs, so
/// a failing assertion never fails the story (Vitest 5 fails unawaited assertions for the same reason). The compiler warns
/// about this in async functions (CS4014) but not in synchronous ones, such as a lambda that returns <c>Task.CompletedTask</c>
/// or a helper method that returns <c>void</c>; this analyzer covers those.
/// </summary>
[DiagnosticAnalyzer(LanguageNames.CSharp)]
public sealed class PlayAnalyzer : DiagnosticAnalyzer
{
    private const string HarnessType = "BlazorTestPlatform.ComponentHarness";

    /// <summary>BTP006: a play call returning a task is used as a statement in a synchronous function.</summary>
    public static readonly DiagnosticDescriptor UnawaitedPlayCall = new(
        "BTP006",
        "Await the assertion or action",
        "'{0}' returns a task that is not awaited: the play script goes on without it, so a failure here never fails the story. Await it.",
        "BlazorTestPlatform.Play",
        DiagnosticSeverity.Warning,
        isEnabledByDefault: true,
        description: "Assertions, actions and harness calls run asynchronously. Awaiting them is what makes the story wait for the check and fail when it fails. Discard the task explicitly (_ = ...) when not waiting is intended.");

    /// <inheritdoc />
    public override ImmutableArray<DiagnosticDescriptor> SupportedDiagnostics { get; } = ImmutableArray.Create(UnawaitedPlayCall);

    /// <inheritdoc />
    public override void Initialize(AnalysisContext context)
    {
        context.ConfigureGeneratedCodeAnalysis(GeneratedCodeAnalysisFlags.None);
        context.EnableConcurrentExecution();
        context.RegisterCompilationStartAction(start =>
        {
            var task = start.Compilation.GetTypeByMetadataName("System.Threading.Tasks.Task");
            var harness = start.Compilation.GetTypeByMetadataName(HarnessType);
            if (task is null || start.Compilation.GetTypeByMetadataName("BlazorTestPlatform.StoryContext") is null)
            {
                return;
            }

            start.RegisterOperationAction(operation => Analyze(operation, task, harness), OperationKind.ExpressionStatement);
        });
    }

    private static void Analyze(OperationAnalysisContext context, INamedTypeSymbol task, INamedTypeSymbol? harness)
    {
        var statement = (IExpressionStatementOperation)context.Operation;
        if (statement.Operation is not IInvocationOperation invocation || !ReturnsTask(invocation.TargetMethod.ReturnType, task))
        {
            return;
        }

        var method = invocation.TargetMethod;
        if (!IsPlatformMember(method.ContainingType, harness) || IsInAsyncFunction(statement, context.ContainingSymbol))
        {
            return;
        }

        context.ReportDiagnostic(Diagnostic.Create(UnawaitedPlayCall, invocation.Syntax.GetLocation(), method.Name));
    }

    private static bool ReturnsTask(ITypeSymbol type, INamedTypeSymbol task)
    {
        for (var current = type as INamedTypeSymbol; current is not null; current = current.BaseType)
        {
            if (SymbolEqualityComparer.Default.Equals(current.OriginalDefinition, task))
            {
                return true;
            }
        }

        return false;
    }

    // The assemblies whose task-returning members are play calls (the story context, locators, assertions, drivers).
    private static readonly ImmutableHashSet<string> PlatformAssemblies = ImmutableHashSet.Create(
        "BlazorTestPlatform.Core", "BlazorTestPlatform.Bunit", "BlazorTestPlatform.Playwright", "BlazorTestPlatform.Workbench");

    /// <summary>
    /// Whether members of <paramref name="type"/> are play calls: types of the platform's own assemblies, and component
    /// harnesses (types derived from <c>ComponentHarness</c>). Helpers in projects that only share the namespace are not.
    /// </summary>
    public static bool IsPlatformMember(INamedTypeSymbol? type, INamedTypeSymbol? harness)
    {
        if (type is null)
        {
            return false;
        }

        if (type.ContainingAssembly is { } assembly && PlatformAssemblies.Contains(assembly.Name))
        {
            return true;
        }

        for (var current = type.BaseType; harness is not null && current is not null; current = current.BaseType)
        {
            if (SymbolEqualityComparer.Default.Equals(current, harness))
            {
                return true;
            }
        }

        return false;
    }

    // CS4014 already covers async functions: report only in synchronous ones.
    private static bool IsInAsyncFunction(IOperation operation, ISymbol containing)
    {
        for (var parent = operation.Parent; parent is not null; parent = parent.Parent)
        {
            switch (parent)
            {
                case IAnonymousFunctionOperation lambda:
                    return lambda.Symbol.IsAsync;
                case ILocalFunctionOperation local:
                    return local.Symbol.IsAsync;
            }
        }

        return containing is IMethodSymbol { IsAsync: true };
    }
}
