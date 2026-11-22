using System.Collections.Immutable;
using System.Composition;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using BlazorTestPlatform.Generators;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CodeActions;
using Microsoft.CodeAnalysis.CodeFixes;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;
using Microsoft.CodeAnalysis.Formatting;

namespace BlazorTestPlatform.CodeFixes;

/// <summary>
/// BTP006: awaits the dropped assertions and actions of a function and makes it async (<c>return Task.CompletedTask;</c>
/// becomes <c>return;</c>, and a <c>void</c> method returns <c>Task</c>). Only platform calls are awaited, so deliberate
/// fire-and-forget calls stay as they are. No fix is offered where async would not compile or would change the contract:
/// other return values, <c>ref</c>/<c>out</c> parameters, iterators, partial methods, overrides, interface
/// implementations, methods used as delegates, and lambdas whose delegate does not return a task (they would become
/// <c>async void</c>, whose failures nobody awaits).
/// </summary>
[ExportCodeFixProvider(LanguageNames.CSharp, Name = nameof(PlayCodeFixProvider)), Shared]
public sealed class PlayCodeFixProvider : CodeFixProvider
{
    /// <inheritdoc />
    public override ImmutableArray<string> FixableDiagnosticIds { get; } = ImmutableArray.Create(PlayAnalyzer.UnawaitedPlayCall.Id);

    /// <inheritdoc />
    public override FixAllProvider GetFixAllProvider() => WellKnownFixAllProviders.BatchFixer;

    /// <inheritdoc />
    public override async Task RegisterCodeFixesAsync(CodeFixContext context)
    {
        var root = await context.Document.GetSyntaxRootAsync(context.CancellationToken).ConfigureAwait(false);
        var model = await context.Document.GetSemanticModelAsync(context.CancellationToken).ConfigureAwait(false);
        if (root is null || model is null)
        {
            return;
        }

        foreach (var diagnostic in context.Diagnostics)
        {
            var invocation = root.FindNode(diagnostic.Location.SourceSpan, getInnermostNodeForTie: true).FirstAncestorOrSelf<InvocationExpressionSyntax>();
            var function = invocation?.Ancestors().FirstOrDefault(n => n is AnonymousFunctionExpressionSyntax or LocalFunctionStatementSyntax or MethodDeclarationSyntax);
            if (invocation is null || function is null || !CanMakeAsync(function, root, model, context.CancellationToken))
            {
                continue;
            }

            context.RegisterCodeFix(
                CodeAction.Create("Await it (make the function async)", ct => FixAsync(context.Document, root, function, ct), "BTP006"),
                diagnostic);
        }
    }

    private static bool CanMakeAsync(SyntaxNode function, SyntaxNode root, SemanticModel model, CancellationToken cancellationToken)
    {
        var own = OwnNodes(function).ToArray();

        // Returns with a value other than Task.CompletedTask would change meaning; iterators cannot be async.
        if (own.OfType<ReturnStatementSyntax>().Any(r => r.Expression is not null && !IsCompletedTask(r.Expression)) || own.OfType<YieldStatementSyntax>().Any())
        {
            return false;
        }

        switch (function)
        {
            case AnonymousFunctionExpressionSyntax lambda:
                // Only a lambda whose delegate returns a task: otherwise it would become async void.
                return model.GetTypeInfo(lambda, cancellationToken).ConvertedType is INamedTypeSymbol { DelegateInvokeMethod: { } invoke }
                    && invoke.ReturnType is INamedTypeSymbol returned
                    && returned.OriginalDefinition.ToDisplayString() is "System.Threading.Tasks.Task" or "System.Threading.Tasks.Task<TResult>";
            case LocalFunctionStatementSyntax local:
                return !HasRefOrOut(local.ParameterList) && (!IsVoid(local.ReturnType) || !UsedAsDelegate(local.Identifier.ValueText, root));
            case MethodDeclarationSyntax method:
                if (HasRefOrOut(method.ParameterList) || method.Modifiers.Any(SyntaxKind.PartialKeyword) || model.GetDeclaredSymbol(method, cancellationToken) is not { } symbol)
                {
                    return false;
                }

                // Changing void to Task would break an override, an interface implementation or a delegate use.
                return !IsVoid(method.ReturnType)
                    || (!symbol.IsOverride && symbol.ExplicitInterfaceImplementations.IsEmpty && !ImplementsInterface(symbol) && !UsedAsDelegate(symbol.Name, root));
            default:
                return false;
        }
    }

    private static bool ImplementsInterface(IMethodSymbol method) =>
        method.ContainingType.AllInterfaces.SelectMany(i => i.GetMembers().OfType<IMethodSymbol>())
            .Any(m => SymbolEqualityComparer.Default.Equals(method.ContainingType.FindImplementationForInterfaceMember(m), method));

    // A method group anywhere in the document (Handler without a call): an event subscription or a delegate.
    private static bool UsedAsDelegate(string name, SyntaxNode root) =>
        root.DescendantNodes().OfType<IdentifierNameSyntax>()
            .Any(n => n.Identifier.ValueText == name
                && n.Parent is not InvocationExpressionSyntax
                && !(n.Parent is MemberAccessExpressionSyntax access && access.Name == n && access.Parent is InvocationExpressionSyntax));

    private static bool HasRefOrOut(ParameterListSyntax parameters) =>
        parameters.Parameters.Any(p => p.Modifiers.Any(m => m.IsKind(SyntaxKind.RefKeyword) || m.IsKind(SyntaxKind.OutKeyword) || m.IsKind(SyntaxKind.InKeyword)));

    private static bool IsVoid(TypeSyntax type) => type is PredefinedTypeSyntax { Keyword.RawKind: (int)SyntaxKind.VoidKeyword };

    private static bool IsCompletedTask(ExpressionSyntax expression) =>
        expression is MemberAccessExpressionSyntax { Name.Identifier.ValueText: "CompletedTask", Expression: var target }
        && target.ToString() is "Task" or "System.Threading.Tasks.Task";

    // The function's own statements, not those of functions nested in it.
    private static System.Collections.Generic.IEnumerable<SyntaxNode> OwnNodes(SyntaxNode function) =>
        function.DescendantNodes(n => n == function || n is not (AnonymousFunctionExpressionSyntax or LocalFunctionStatementSyntax));

    private static async Task<Document> FixAsync(Document document, SyntaxNode root, SyntaxNode function, CancellationToken cancellationToken)
    {
        var model = await document.GetSemanticModelAsync(cancellationToken).ConfigureAwait(false);
        var task = model?.Compilation.GetTypeByMetadataName("System.Threading.Tasks.Task");
        var harness = model?.Compilation.GetTypeByMetadataName("BlazorTestPlatform.ComponentHarness");

        // The platform calls the function drops (what BTP006 reports); other tasks may be fire-and-forget on purpose.
        var own = OwnNodes(function).ToArray();
        var dropped = own.OfType<InvocationExpressionSyntax>()
            .Where(i => i.Parent is ExpressionStatementSyntax or ArrowExpressionClauseSyntax
                && model?.GetSymbolInfo(i, cancellationToken).Symbol is IMethodSymbol method
                && IsTask(method.ReturnType, task)
                && PlayAnalyzer.IsPlatformMember(method.ContainingType, harness))
            .ToArray();
        var updated = function.ReplaceNodes(
            dropped.Cast<SyntaxNode>().Concat(own.OfType<ReturnStatementSyntax>().Where(r => r.Expression is not null)),
            (original, _) => original is InvocationExpressionSyntax call
                ? SyntaxFactory.AwaitExpression(call.WithoutTrivia()).WithTriviaFrom(call)
                : SyntaxFactory.ReturnStatement().WithTriviaFrom(original));

        updated = updated switch
        {
            AnonymousFunctionExpressionSyntax lambda => lambda.WithAsyncKeyword(SyntaxFactory.Token(SyntaxKind.AsyncKeyword).WithTrailingTrivia(SyntaxFactory.Space)).WithLeadingTrivia(lambda.GetLeadingTrivia()),
            LocalFunctionStatementSyntax local => AddAsync(local.Modifiers, local.ReturnType, (m, t) => local.WithModifiers(m).WithReturnType(t)),
            MethodDeclarationSyntax method => AddAsync(method.Modifiers, method.ReturnType, (m, t) => method.WithModifiers(m).WithReturnType(t)),
            _ => updated,
        };

        var newRoot = root.ReplaceNode(function, updated.WithAdditionalAnnotations(Formatter.Annotation));
        return document.WithSyntaxRoot(newRoot);
    }

    // Adds async after the existing modifiers, or before the return type with its indentation moved onto async.
    private static SyntaxNode AddAsync(SyntaxTokenList modifiers, TypeSyntax returnType, System.Func<SyntaxTokenList, TypeSyntax, SyntaxNode> apply)
    {
        var task = AsTask(returnType);
        if (modifiers.Count > 0)
        {
            return apply(modifiers.Add(SyntaxFactory.Token(SyntaxKind.AsyncKeyword).WithTrailingTrivia(SyntaxFactory.Space)), task);
        }

        var asyncToken = SyntaxFactory.Token(task.GetLeadingTrivia(), SyntaxKind.AsyncKeyword, SyntaxFactory.TriviaList(SyntaxFactory.Space));
        return apply(SyntaxFactory.TokenList(asyncToken), task.WithLeadingTrivia());
    }

    // void becomes Task (async void cannot be awaited by the caller); Task stays.
    private static TypeSyntax AsTask(TypeSyntax returnType) =>
        IsVoid(returnType) ? SyntaxFactory.IdentifierName("Task").WithTriviaFrom(returnType) : returnType;

    private static bool IsTask(ITypeSymbol? type, INamedTypeSymbol? task)
    {
        for (var current = type as INamedTypeSymbol; task is not null && current is not null; current = current.BaseType)
        {
            if (SymbolEqualityComparer.Default.Equals(current.OriginalDefinition, task))
            {
                return true;
            }
        }

        return false;
    }
}
