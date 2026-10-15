using System.Runtime.CompilerServices;
using System.Text.Json;
using Daedalus.ManagedHost;
using Daedalus.ManagedSdk;

namespace Daedalus.ManagedIntegrationTests;

internal static class Program
{
    private static int Main()
    {
        try
        {
            NativeWorldRoundTripUsesRustState();
            ModuleReplacementMigratesAndRollsBack();
            Console.WriteLine("PASS managed/native integration: 2 scenarios");
            return 0;
        }
        catch (Exception exception)
        {
            Console.Error.WriteLine($"FAIL {exception.GetType().Name}: {exception.Message}");
            return 1;
        }
    }

    private static void NativeWorldRoundTripUsesRustState()
    {
        using var engine = NativeEngineSession.Create();
        var entity = engine.SpawnTransform(TransformState.Identity.WithTranslationX(4.0f));
        Check(engine.GetEntities().SequenceEqual([entity]), "entity listing must return the Rust-owned identifier");

        var before = engine.GetTransform(entity);
        Check(before.Revision == 0, "new transform begins at Rust revision zero");
        var commandId = engine.SetTransform(entity, before, before.Transform.WithTranslationX(5.5f));
        var report = engine.Step();
        var outcome = report.Commands.Single(command => command.CommandId == commandId);
        Check(outcome.Applied, "Rust StepReport acknowledges the command");

        var after = engine.GetTransform(entity);
        Check(after.Entity == entity, "query returns the same entity handle");
        Check(after.Transform.Translation[0] == 5.5f, "query returns the transform stored in Rust ECS");
        Check(after.Revision == 1, "Rust revision advances after the command is applied");

        using var snapshot = engine.LatestSnapshot();
        var snapshotData = snapshot.RootElement.GetProperty("results")[0]
            .GetProperty("result").GetProperty("snapshot");
        Check(snapshotData.GetProperty("tick").GetUInt64() == 1, "latest presentation snapshot comes from Rust tick one");
        Check(snapshotData.GetProperty("entities").GetArrayLength() == 1, "snapshot contains the Rust-owned entity");

        var stale = entity with { Generation = checked(entity.Generation + 100) };
        var error = Throws<NativeBridgeException>(() => engine.GetTransform(stale));
        Check(error.Code == "entity_read_failed", "stale Rust entity handles surface a concrete native error");
        Throws<ArgumentException>(() => TransformState.Identity.WithTranslationX(float.PositiveInfinity).Validate());
        Console.WriteLine("PASS native round-trip: Rust ID, transform, revision, StepReport, and snapshot");
    }

    private static void ModuleReplacementMigratesAndRollsBack()
    {
        var repositoryRoot = FindRepositoryRoot();
        var configuration = new DirectoryInfo(AppContext.BaseDirectory.TrimEnd(Path.DirectorySeparatorChar))
            .Parent?.Name ?? "Release";
        var fixtureRoot = Path.Combine(repositoryRoot, "managed", "fixtures");
        var temporaryRoot = Path.Combine(Path.GetTempPath(), $"daedalus-module-fixtures-{Guid.NewGuid():N}");
        Directory.CreateDirectory(temporaryRoot);

        try
        {
            var v1 = StageFixture(fixtureRoot, configuration, temporaryRoot, "Daedalus.Fixture.V1", "1.0.0");
            var v2 = StageFixture(fixtureRoot, configuration, temporaryRoot, "Daedalus.Fixture.V2", "2.0.0");
            var broken = StageFixture(fixtureRoot, configuration, temporaryRoot, "Daedalus.Fixture.Broken", "2.1.0");

            using var engine = NativeEngineSession.Create();
            var entity = engine.SpawnTransform(TransformState.Identity);
            using var modules = new ModuleManager(engine);

            RequireSuccess(modules.Replace(v1.ManifestPath));
            Check(modules.CurrentVersion == "1.0.0", "initial V1 module is active");
            var v1Tick = modules.Tick();
            Check(v1Tick.Success && v1Tick.CommandIds.Count == 1, "V1 callback coalesces multiple staged writes into one command");
            RequireApplied(v1Tick.CommandIds, engine.Step());
            var beforeReload = engine.GetTransform(entity);
            Check(beforeReload.Transform.Translation[0] == 1.0f, "V1 behavior moved the Rust entity by one unit");

            var v2Reload = modules.Replace(v2.ManifestPath);
            RequireSuccess(v2Reload);
            var afterReload = engine.GetTransform(entity);
            Check(afterReload.Entity == entity, "replacement preserves the native entity ID and generation");
            Check(afterReload.Transform.Translation[0] == beforeReload.Transform.Translation[0], "successful migration preserves native position");
            Check(afterReload.Revision == beforeReload.Revision + 1, "V2 staged activation was acknowledged by a Rust tick");
            Check(modules.CleanedCallbackCount == 1, "retiring V1 ran and cleared its generation cleanup callback");

            var failedReload = modules.Replace(broken.ManifestPath);
            Check(!failedReload.Success, "broken migration is rejected");
            Check(failedReload.Error?.Contains("fixture migration failure", StringComparison.Ordinal) == true,
                "migration diagnostics preserve the managed failure detail");
            Check(modules.CurrentVersion == "2.0.0", "failed migration retains the previously active V2 module");
            Check(modules.CleanedCallbackCount == 2,
                "failed candidate migration rolls back its registered cleanup without retiring V2");
            var afterFailure = engine.GetTransform(entity);
            Check(afterFailure.Entity == entity && afterFailure.Transform.Translation[0] == 1.0f,
                "failed candidate's staged x=999 mutation never reached the Rust world");
            Check(afterFailure.Revision == afterReload.Revision, "failed migration does not bump Rust revision");

            var failedTick = modules.Tick();
            Check(!failedTick.Success && failedTick.Error?.Contains("fixture tick failure", StringComparison.Ordinal) == true,
                "a failing callback is reported without committing its staged effects");
            Check(modules.CleanedCallbackCount == 3,
                "failed callback rolls back only its newly registered cleanup callback");

            var v2Tick = modules.Tick();
            Check(v2Tick.Success && v2Tick.CommandIds.Count == 1, "old V2 logic remains callable after failure");
            RequireApplied(v2Tick.CommandIds, engine.Step());
            var migratedBehavior = engine.GetTransform(entity);
            Check(migratedBehavior.Transform.Translation[0] == 13.0f,
                "V2 logic changed and used V1 state captured/restored across the collectible context");

            ExerciseRepeatedReplacement(modules, v1.ManifestPath, v2.ManifestPath, 6);
            Check(modules.UnloadedContextCount >= 14, "each retired or rejected module generation is tracked weakly");
            modules.Dispose();
            var stillAlive = modules.CollectUnloadedContexts(maxPasses: 40);
            Check(stillAlive == 0, $"collectible AssemblyLoadContexts should unload; live count was {stillAlive}");
            Check(modules.CleanedCallbackCount >= 15, "generation cleanup callbacks run across repeated replacement");
            Console.WriteLine("PASS module lifecycle: state migration, staged rollback, old logic retention, repeated ALC unload");
        }
        finally
        {
            try
            {
                Directory.Delete(temporaryRoot, recursive: true);
            }
            catch (IOException)
            {
                // macOS may defer deleting a just-unloaded assembly image; this is fixture cleanup only.
            }
        }
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static void ExerciseRepeatedReplacement(ModuleManager modules, string v1, string v2, int cycles)
    {
        for (var cycle = 0; cycle < cycles; cycle++)
        {
            RequireSuccess(modules.Replace(v1));
            RequireSuccess(modules.Replace(v2));
        }
    }

    private static StagedFixture StageFixture(
        string fixtureRoot,
        string configuration,
        string temporaryRoot,
        string projectName,
        string version)
    {
        var projectOutput = Path.Combine(fixtureRoot, projectName, "bin", configuration, "net10.0");
        if (!Directory.Exists(projectOutput))
        {
            throw new DirectoryNotFoundException($"fixture output does not exist: {projectOutput}");
        }

        var stagePath = Path.Combine(temporaryRoot, projectName);
        Directory.CreateDirectory(stagePath);
        foreach (var file in Directory.EnumerateFiles(projectOutput))
        {
            File.Copy(file, Path.Combine(stagePath, Path.GetFileName(file)), overwrite: true);
        }

        var className = projectName switch
        {
            "Daedalus.Fixture.V1" => "Daedalus.Fixture.V1.TransformModule",
            "Daedalus.Fixture.V2" => "Daedalus.Fixture.V2.TransformModule",
            "Daedalus.Fixture.Broken" => "Daedalus.Fixture.Broken.TransformModule",
            _ => throw new ArgumentOutOfRangeException(nameof(projectName)),
        };
        var manifestPath = Path.Combine(stagePath, "module.json");
        var manifest = new
        {
            id = "transform-fixture",
            version,
            sdkApiVersion = 1,
            entryType = className,
            assembly = $"{projectName}.dll",
        };
        File.WriteAllBytes(manifestPath, JsonSerializer.SerializeToUtf8Bytes(manifest));
        Check(File.Exists(Path.Combine(stagePath, $"{projectName}.deps.json")), "fixture has dependency metadata for AssemblyDependencyResolver");
        return new StagedFixture(manifestPath);
    }

    private static string FindRepositoryRoot()
    {
        var directory = new DirectoryInfo(Environment.CurrentDirectory);
        while (directory is not null)
        {
            if (File.Exists(Path.Combine(directory.FullName, "global.json")))
            {
                return directory.FullName;
            }
            directory = directory.Parent;
        }
        throw new DirectoryNotFoundException("could not locate repository global.json from the current working directory");
    }

    private static void RequireApplied(IReadOnlyCollection<string> commandIds, StepResult report)
    {
        var outcomes = report.Commands.ToDictionary(static command => command.CommandId, StringComparer.Ordinal);
        foreach (var commandId in commandIds)
        {
            Check(outcomes.TryGetValue(commandId, out var command) && command.Applied,
                command?.Error ?? $"StepReport omitted command {commandId}");
        }
    }

    private static void RequireSuccess(ModuleReloadResult result) =>
        Check(result.Success, result.Error ?? "module replacement failed without diagnostic");

    private static TException Throws<TException>(Action action) where TException : Exception
    {
        try
        {
            action();
        }
        catch (TException exception)
        {
            return exception;
        }
        throw new InvalidOperationException($"expected {typeof(TException).Name} was not thrown");
    }

    private static void Check(bool condition, string message)
    {
        if (!condition)
        {
            throw new InvalidOperationException(message);
        }
    }

    private sealed record StagedFixture(string ManifestPath);
}
