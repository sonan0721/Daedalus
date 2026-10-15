using System.Reflection;
using System.Runtime.CompilerServices;
using System.Runtime.Loader;
using System.Text.Json;
using System.Text.Json.Serialization;
using Daedalus.ManagedSdk;

namespace Daedalus.ManagedHost;

public sealed record ModuleReloadResult(
    bool Success,
    string? ModuleId,
    string? Version,
    long Generation,
    string? Error,
    IReadOnlyList<string> Warnings);

public sealed record ModuleTickResult(bool Success, IReadOnlyList<string> CommandIds, string? Error);

/// <summary>
/// Performs trusted full-module replacements. Callers must invoke replacement at a simulation
/// safe point; this initial manager is synchronous and is not a sandbox or MetadataUpdater host.
/// </summary>
public sealed class ModuleManager : IDisposable
{
    private const string ManagedSdkAssemblyName = "Daedalus.ManagedSdk";
    private const int MaxManifestBytes = 65_536;
    private const int MaxStateBytes = 1_048_576;
    private const int MaxCleanupCallbacksPerGeneration = 4096;
    private const int MaxPendingUnloadedContexts = 256;
    private static readonly JsonSerializerOptions ManifestJsonOptions = new(JsonSerializerDefaults.Web)
    {
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
    };

    private readonly NativeEngineSession _engine;
    private readonly List<WeakReference> _pendingUnloadedContexts = [];
    private PluginGeneration? _current;
    private long _nextGeneration = 1;
    private int _unloadedContextCount;
    private bool _disposed;

    public ModuleManager(NativeEngineSession engine)
    {
        _engine = engine;
    }

    public string? CurrentModuleId => _current?.Manifest.Id;

    public string? CurrentVersion => _current?.Manifest.Version;

    public long CurrentGeneration => _current?.Generation ?? 0;

    public int CleanedCallbackCount { get; private set; }

    /// <summary>Cumulative number of collectible module contexts asked to unload.</summary>
    public int UnloadedContextCount => _unloadedContextCount;

    public int LiveUnloadedContextCount
    {
        get
        {
            PruneUnloadedContexts();
            return _pendingUnloadedContexts.Count;
        }
    }

    public ModuleReloadResult Replace(string manifestPath)
    {
        ThrowIfDisposed();
        PruneUnloadedContexts();
        // Keep one slot for the active generation's eventual disposal; this operation can
        // retire either the prior active generation or a rejected candidate, but not both.
        if (_pendingUnloadedContexts.Count >= MaxPendingUnloadedContexts - 1)
        {
            return Failed(
                "unload_tracking_limit",
                new InvalidOperationException(
                    $"retired module context tracking is at its {MaxPendingUnloadedContexts - 1} pending-generation threshold; collect them before replacing another module"),
                _current);
        }

        var previous = _current;
        var generation = _nextGeneration;
        long nextGeneration;
        try
        {
            // Reserve the next value before candidate callbacks can stage a world mutation.
            nextGeneration = checked(generation + 1);
        }
        catch (OverflowException exception)
        {
            return Failed("generation_exhausted", exception, previous);
        }

        byte[] state;
        try
        {
            state = previous is null ? [] : previous.Module.CaptureState();
            if (state is null || state.Length > MaxStateBytes)
            {
                throw new InvalidOperationException("module state must be non-null and no larger than 1 MiB");
            }
            state = (byte[])state.Clone();
        }
        catch (Exception exception)
        {
            return Failed("capture_state_failed", exception, previous);
        }

        PluginGeneration candidate;
        try
        {
            candidate = LoadCandidate(manifestPath, generation);
        }
        catch (Exception exception)
        {
            return Failed("candidate_load_failed", exception, previous);
        }

        var context = new StagingContext(_engine, candidate.Scope);
        try
        {
            candidate.Module.RestoreState(state, context);
            candidate.Module.OnStart(context);
            var commandIds = context.Commit();
            _engine.ConfirmApplied(commandIds);
        }
        catch (Exception exception)
        {
            var cleanupWarnings = context.Discard();
            Retire(candidate, callModuleUnload: true);
            var failure = Failed("candidate_activation_failed", exception, previous);
            return cleanupWarnings.Count == 0
                ? failure
                : failure with { Error = $"{failure.Error}; {string.Join("; ", cleanupWarnings)}" };
        }

        _nextGeneration = nextGeneration;
        _current = candidate;
        var warnings = previous is null ? Array.Empty<string>() : Retire(previous, callModuleUnload: true);
        return new ModuleReloadResult(
            true,
            candidate.Manifest.Id,
            candidate.Manifest.Version,
            candidate.Generation,
            null,
            warnings);
    }

    public ModuleTickResult Tick()
    {
        ThrowIfDisposed();
        if (_current is null)
        {
            return new ModuleTickResult(false, [], "no gameplay module is active");
        }

        var context = new StagingContext(_engine, _current.Scope);
        try
        {
            _current.Module.OnTick(context);
            var commandIds = context.Commit();
            return new ModuleTickResult(true, commandIds, null);
        }
        catch (Exception exception)
        {
            var cleanupWarnings = context.Discard();
            var error = DescribeException(exception);
            if (cleanupWarnings.Count > 0)
            {
                error = $"{error}; {string.Join("; ", cleanupWarnings)}";
            }
            return new ModuleTickResult(false, [], error);
        }
    }

    /// <summary>Run bounded full collections to observe cooperative ALC unload progress.</summary>
    public int CollectUnloadedContexts(int maxPasses = 10)
    {
        if (maxPasses is < 1 or > 100)
        {
            throw new ArgumentOutOfRangeException(nameof(maxPasses), "collection passes must be between 1 and 100");
        }

        for (var pass = 0; pass < maxPasses && LiveUnloadedContextCount > 0; pass++)
        {
            GC.Collect();
            GC.WaitForPendingFinalizers();
            GC.Collect();
            Thread.Yield();
        }
        return LiveUnloadedContextCount;
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }
        _disposed = true;
        var active = _current;
        _current = null;
        if (active is not null)
        {
            _ = Retire(active, callModuleUnload: true);
        }
    }

    private PluginGeneration LoadCandidate(string manifestPath, long generation)
    {
        var manifest = ReadManifest(manifestPath);
        var pluginRoot = Path.GetDirectoryName(Path.GetFullPath(manifestPath))
            ?? throw new InvalidOperationException("manifest path has no parent directory");
        var assemblyPath = ResolveInside(pluginRoot, manifest.Assembly);
        if (!File.Exists(assemblyPath))
        {
            throw new FileNotFoundException($"entry assembly '{manifest.Assembly}' does not exist", assemblyPath);
        }

        var loadContext = new PluginLoadContext(assemblyPath);
        try
        {
            var assembly = loadContext.LoadFromAssemblyPath(assemblyPath);
            var entryType = assembly.GetType(manifest.EntryType, throwOnError: true, ignoreCase: false)!;
            if (!typeof(IGameplayModule).IsAssignableFrom(entryType)
                || entryType.IsAbstract
                || entryType.ContainsGenericParameters)
            {
                throw new InvalidOperationException(
                    $"entry type '{manifest.EntryType}' must be a concrete {nameof(IGameplayModule)}");
            }
            if (Activator.CreateInstance(entryType) is not IGameplayModule module)
            {
                throw new InvalidOperationException($"entry type '{manifest.EntryType}' needs a public parameterless constructor");
            }
            if (!string.Equals(module.Id, manifest.Id, StringComparison.Ordinal))
            {
                throw new InvalidOperationException(
                    $"manifest module ID '{manifest.Id}' does not match entry module ID '{module.Id}'");
            }

            return new PluginGeneration(
                manifest,
                generation,
                loadContext,
                module,
                new GenerationScope(() => CleanedCallbackCount++));
        }
        catch
        {
            loadContext.Unload();
            TrackUnloadedContext(loadContext);
            throw;
        }
    }

    private static PluginManifest ReadManifest(string path)
    {
        var fullPath = Path.GetFullPath(path);
        var info = new FileInfo(fullPath);
        if (!info.Exists)
        {
            throw new FileNotFoundException("module manifest does not exist", fullPath);
        }
        if (info.Length is < 2 or > MaxManifestBytes)
        {
            throw new InvalidDataException("module manifest must be between 2 bytes and 64 KiB");
        }
        var bytes = File.ReadAllBytes(fullPath);
        var manifest = JsonSerializer.Deserialize<PluginManifest>(bytes, ManifestJsonOptions)
            ?? throw new InvalidDataException("module manifest is empty");
        if (string.IsNullOrWhiteSpace(manifest.Id)
            || string.IsNullOrWhiteSpace(manifest.Version)
            || string.IsNullOrWhiteSpace(manifest.EntryType)
            || string.IsNullOrWhiteSpace(manifest.Assembly)
            || manifest.SdkApiVersion != 1)
        {
            throw new InvalidDataException("manifest requires id, version, entryType, assembly, and sdkApiVersion 1");
        }
        if (Path.IsPathRooted(manifest.Assembly))
        {
            throw new InvalidDataException("manifest assembly path must be relative to its module directory");
        }
        return manifest;
    }

    private static string ResolveInside(string root, string relativePath)
    {
        var fullRoot = Path.GetFullPath(root).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
        var fullPath = Path.GetFullPath(Path.Combine(root, relativePath));
        var comparison = OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal;
        if (!fullPath.StartsWith(fullRoot, comparison))
        {
            throw new InvalidDataException("manifest assembly path resolves outside its module directory");
        }
        return fullPath;
    }

    private IReadOnlyList<string> Retire(PluginGeneration generation, bool callModuleUnload)
    {
        var warnings = new List<string>();
        try
        {
            if (callModuleUnload)
            {
                try
                {
                    generation.Module.OnUnload();
                }
                catch (Exception exception)
                {
                    warnings.Add($"module unload callback failed: {DescribeException(exception)}");
                }
            }
        }
        finally
        {
            generation.Scope.Dispose(warnings);
            var weakContext = new WeakReference(generation.LoadContext, trackResurrection: false);
            generation.LoadContext.Unload();
            TrackUnloadedContext(weakContext);
        }
        return warnings;
    }

    private void TrackUnloadedContext(AssemblyLoadContext context) =>
        TrackUnloadedContext(new WeakReference(context, trackResurrection: false));

    private void TrackUnloadedContext(WeakReference context)
    {
        PruneUnloadedContexts();
        if (_pendingUnloadedContexts.Count >= MaxPendingUnloadedContexts)
        {
            throw new InvalidOperationException("retired module context tracking capacity was not reserved");
        }
        _pendingUnloadedContexts.Add(context);
        _unloadedContextCount = _unloadedContextCount == int.MaxValue
            ? int.MaxValue
            : _unloadedContextCount + 1;
    }

    private void PruneUnloadedContexts() =>
        _pendingUnloadedContexts.RemoveAll(static weak => !weak.IsAlive);

    private static ModuleReloadResult Failed(string stage, Exception exception, PluginGeneration? previous) =>
        new(
            false,
            previous?.Manifest.Id,
            previous?.Manifest.Version,
            previous?.Generation ?? 0,
            $"{stage}: {DescribeException(exception)}",
            []);

    private static string DescribeException(Exception exception)
    {
        var root = exception;
        while (root is TargetInvocationException { InnerException: not null } invocation)
        {
            root = invocation.InnerException!;
        }
        return $"{root.GetType().Name}: {root.Message}";
    }

    private void ThrowIfDisposed()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
    }

    private sealed record PluginManifest(
        string Id,
        string Version,
        int SdkApiVersion,
        string EntryType,
        string Assembly);

    private sealed class PluginGeneration(
        PluginManifest manifest,
        long generation,
        PluginLoadContext loadContext,
        IGameplayModule module,
        GenerationScope scope)
    {
        internal PluginManifest Manifest { get; } = manifest;
        internal long Generation { get; } = generation;
        internal PluginLoadContext LoadContext { get; } = loadContext;
        internal IGameplayModule Module { get; } = module;
        internal GenerationScope Scope { get; } = scope;
    }

    private sealed class PluginLoadContext : AssemblyLoadContext
    {
        private readonly AssemblyDependencyResolver _resolver;

        internal PluginLoadContext(string assemblyPath)
            : base($"daedalus-module:{Path.GetFileNameWithoutExtension(assemblyPath)}:{Guid.NewGuid():N}", isCollectible: true)
        {
            _resolver = new AssemblyDependencyResolver(assemblyPath);
        }

        protected override Assembly? Load(AssemblyName assemblyName)
        {
            if (string.Equals(assemblyName.Name, ManagedSdkAssemblyName, StringComparison.Ordinal))
            {
                return null;
            }
            var path = _resolver.ResolveAssemblyToPath(assemblyName);
            return path is null ? null : LoadFromAssemblyPath(path);
        }

        protected override nint LoadUnmanagedDll(string unmanagedDllName)
        {
            var path = _resolver.ResolveUnmanagedDllToPath(unmanagedDllName);
            return path is null ? nint.Zero : LoadUnmanagedDllFromPath(path);
        }
    }

    private sealed class GenerationScope(Action callbackCleaned)
    {
        private List<Action>? _cleanups = [];

        internal void Register(Action cleanup)
        {
            ArgumentNullException.ThrowIfNull(cleanup);
            var cleanups = _cleanups ?? throw new ObjectDisposedException(nameof(GenerationScope));
            if (cleanups.Count >= MaxCleanupCallbacksPerGeneration)
            {
                throw new InvalidOperationException(
                    $"generation cleanup callback limit ({MaxCleanupCallbacksPerGeneration}) reached");
            }
            cleanups.Add(cleanup);
        }

        internal int Checkpoint =>
            (_cleanups ?? throw new ObjectDisposedException(nameof(GenerationScope))).Count;

        internal IReadOnlyList<string> RollbackTo(int checkpoint)
        {
            var cleanups = _cleanups ?? throw new ObjectDisposedException(nameof(GenerationScope));
            if (checkpoint < 0 || checkpoint > cleanups.Count)
            {
                throw new ArgumentOutOfRangeException(nameof(checkpoint));
            }
            var warnings = new List<string>();
            for (var index = cleanups.Count - 1; index >= checkpoint; index--)
            {
                try
                {
                    cleanups[index]();
                }
                catch (Exception exception)
                {
                    warnings.Add($"staged cleanup callback failed: {DescribeException(exception)}");
                }
                finally
                {
                    callbackCleaned();
                }
            }
            cleanups.RemoveRange(checkpoint, cleanups.Count - checkpoint);
            return warnings;
        }

        internal void Dispose(List<string> warnings)
        {
            var cleanups = _cleanups;
            _cleanups = null;
            if (cleanups is null)
            {
                return;
            }
            for (var index = cleanups.Count - 1; index >= 0; index--)
            {
                try
                {
                    cleanups[index]();
                }
                catch (Exception exception)
                {
                    warnings.Add($"generation cleanup callback failed: {DescribeException(exception)}");
                }
                finally
                {
                    callbackCleaned();
                }
            }
            cleanups.Clear();
        }
    }

    private sealed class StagingContext(NativeEngineSession engine, GenerationScope scope) : IGameplayContext
    {
        private readonly Dictionary<EntityHandle, StagedTransform> _staged = [];
        private readonly int _cleanupCheckpoint = scope.Checkpoint;
        private bool _active = true;

        public IReadOnlyList<EntityHandle> GetEntities()
        {
            EnsureActive();
            return engine.GetEntities();
        }

        public EntityState GetTransform(EntityHandle entity)
        {
            EnsureActive();
            if (_staged.TryGetValue(entity, out var staged))
            {
                return new EntityState(entity, staged.Transform, staged.ExpectedRevision);
            }
            return engine.GetTransform(entity);
        }

        public void SetTransform(EntityHandle entity, TransformState transform)
        {
            EnsureActive();
            transform.Validate();
            var expectedRevision = GetTransform(entity).Revision;
            if (_staged.TryGetValue(entity, out var previous))
            {
                expectedRevision = previous.ExpectedRevision;
            }
            else if (_staged.Count >= 256)
            {
                throw new InvalidOperationException("callback transform command limit (256) reached");
            }
            _staged[entity] = new StagedTransform(entity, expectedRevision, transform);
        }

        public void RegisterCleanup(Action cleanup)
        {
            EnsureActive();
            scope.Register(cleanup);
        }

        internal IReadOnlyList<string> Commit()
        {
            EnsureActive();
            _active = false;
            return engine.ApplyTransformBatch(_staged.Values.ToArray());
        }

        internal IReadOnlyList<string> Discard()
        {
            _active = false;
            _staged.Clear();
            return scope.RollbackTo(_cleanupCheckpoint);
        }

        private void EnsureActive()
        {
            if (!_active)
            {
                throw new ObjectDisposedException(nameof(StagingContext), "callback context is valid only during its module callback");
            }
        }
    }
}
