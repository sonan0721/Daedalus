using Daedalus.ManagedSdk;

namespace Daedalus.ManagedHost;

internal static class Program
{
    private static int Main(string[] arguments)
    {
        try
        {
            if (arguments.Length == 0 || arguments is ["--help"] or ["-h"])
            {
                PrintHelp();
                return 0;
            }

            return arguments[0] switch
            {
                "--self-test" when arguments.Length == 1 => SelfTest(),
                "--reload" when arguments.Length is 3 or 4 => Reload(arguments),
                _ => UsageError(),
            };
        }
        catch (Exception exception)
        {
            Console.Error.WriteLine($"error: {exception.GetType().Name}: {exception.Message}");
            return 1;
        }
    }

    private static int SelfTest()
    {
        using var engine = NativeEngineSession.Create();
        var entity = engine.SpawnTransform(TransformState.Identity.WithTranslationX(1.0f));
        var initial = engine.GetTransform(entity);
        var commandId = engine.SetTransform(
            entity,
            initial,
            initial.Transform.WithTranslationX(2.0f));
        var step = engine.Step();
        var applied = step.Commands.SingleOrDefault(command => command.CommandId == commandId);
        if (applied is null || !applied.Applied)
        {
            throw new InvalidOperationException("Rust StepReport did not acknowledge the managed transform command");
        }
        var result = engine.GetTransform(entity);
        Console.WriteLine($"OK ABI={NativeEngineSession.ManagedAbiVersion} entity={Format(entity)} x={result.Transform.Translation[0]} revision={result.Revision}");
        return 0;
    }

    private static int Reload(string[] arguments)
    {
        using var engine = NativeEngineSession.Create();
        var entity = engine.SpawnTransform(TransformState.Identity);
        using var modules = new ModuleManager(engine);

        var initial = modules.Replace(arguments[1]);
        RequireSuccess(initial);
        var tick = modules.Tick();
        if (!tick.Success)
        {
            throw new InvalidOperationException(tick.Error);
        }
        var firstStep = engine.Step();
        RequireApplied(tick.CommandIds, firstStep);

        var before = engine.GetTransform(entity);
        var replacement = modules.Replace(arguments[2]);
        RequireSuccess(replacement);
        var after = engine.GetTransform(entity);

        var postReloadTicks = arguments.Length == 4
            ? int.Parse(arguments[3], System.Globalization.CultureInfo.InvariantCulture)
            : 1;
        if (postReloadTicks is < 0 or > 120)
        {
            throw new ArgumentOutOfRangeException(nameof(arguments), "post-reload ticks must be between 0 and 120");
        }
        for (var index = 0; index < postReloadTicks; index++)
        {
            var moduleTick = modules.Tick();
            if (!moduleTick.Success)
            {
                throw new InvalidOperationException(moduleTick.Error);
            }
            var report = engine.Step();
            RequireApplied(moduleTick.CommandIds, report);
        }
        var final = engine.GetTransform(entity);
        Console.WriteLine(
            $"OK module={modules.CurrentModuleId}@{modules.CurrentVersion} generation={modules.CurrentGeneration} " +
            $"entity={Format(entity)} beforeX={before.Transform.Translation[0]} afterReloadX={after.Transform.Translation[0]} " +
            $"finalX={final.Transform.Translation[0]} revision={final.Revision} retiredContexts={modules.UnloadedContextCount}");
        return 0;
    }

    private static void RequireSuccess(ModuleReloadResult result)
    {
        if (!result.Success)
        {
            throw new InvalidOperationException(result.Error);
        }
    }

    private static void RequireApplied(IReadOnlyCollection<string> commandIds, StepResult report)
    {
        var outcomes = report.Commands.ToDictionary(static command => command.CommandId, StringComparer.Ordinal);
        foreach (var commandId in commandIds)
        {
            if (!outcomes.TryGetValue(commandId, out var command) || !command.Applied)
            {
                throw new InvalidOperationException(command?.Error ?? $"StepReport omitted command {commandId}");
            }
        }
    }

    private static string Format(EntityHandle entity) => $"{entity.Session}:{entity.Index}:{entity.Generation}";

    private static int UsageError()
    {
        PrintHelp();
        return 2;
    }

    private static void PrintHelp()
    {
        Console.WriteLine("Daedalus managed host (.NET 10 / C# 14)");
        Console.WriteLine();
        Console.WriteLine("Usage:");
        Console.WriteLine("  Daedalus.ManagedHost --help");
        Console.WriteLine("  Daedalus.ManagedHost --self-test");
        Console.WriteLine("  Daedalus.ManagedHost --reload <initial-manifest.json> <replacement-manifest.json> [post-reload-ticks]");
        Console.WriteLine();
        Console.WriteLine("Set DAEDALUS_NATIVE_PATH to the built daedalus_bridge dynamic library when it is not on the OS library path.");
        Console.WriteLine("Modules are trusted in-process code; AssemblyLoadContext is a lifetime boundary, not a security sandbox.");
        Console.WriteLine("--reload performs synchronous full-module replacement. MetadataUpdater and compile-on-frame reload are not implemented.");
    }
}
