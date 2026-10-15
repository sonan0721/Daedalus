using System.Reflection;
using System.Runtime.InteropServices;

namespace Daedalus.ManagedHost;

internal static class NativeBridge
{
    private const string LibraryName = "daedalus_bridge";

    static NativeBridge()
    {
        NativeLibrary.SetDllImportResolver(typeof(NativeBridge).Assembly, ResolveLibrary);
    }

    internal static int AbiVersion() => NativeMethods.AbiVersion();

    internal static int CreateEngine(out ulong handle) => NativeMethods.EngineCreate(out handle);

    internal static int DestroyEngine(ulong handle) => NativeMethods.EngineDestroy(handle);

    internal static int Exchange(
        ulong engine,
        nint request,
        nuint requestLength,
        out ulong buffer,
        out nint data,
        out nuint length) =>
        NativeMethods.EngineExchange(engine, request, requestLength, out buffer, out data, out length);

    internal static int ReleaseBuffer(ulong buffer) => NativeMethods.BufferRelease(buffer);

    private static nint ResolveLibrary(string libraryName, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (!string.Equals(libraryName, LibraryName, StringComparison.Ordinal))
        {
            return nint.Zero;
        }

        var configuredPath = Environment.GetEnvironmentVariable("DAEDALUS_NATIVE_PATH");
        if (!string.IsNullOrWhiteSpace(configuredPath))
        {
            var fullPath = Path.GetFullPath(configuredPath);
            if (!NativeLibrary.TryLoad(fullPath, out var configuredHandle))
            {
                throw new DllNotFoundException($"Could not load Daedalus native bridge at '{fullPath}'.");
            }
            return configuredHandle;
        }

        // A zero handle lets the runtime apply normal platform library search rules.
        return nint.Zero;
    }

    private static class NativeMethods
    {
        [DllImport(LibraryName, EntryPoint = "daedalus_bridge_abi_version", CallingConvention = CallingConvention.Cdecl)]
        internal static extern int AbiVersion();

        [DllImport(LibraryName, EntryPoint = "daedalus_bridge_engine_create", CallingConvention = CallingConvention.Cdecl)]
        internal static extern int EngineCreate(out ulong handle);

        [DllImport(LibraryName, EntryPoint = "daedalus_bridge_engine_destroy", CallingConvention = CallingConvention.Cdecl)]
        internal static extern int EngineDestroy(ulong handle);

        [DllImport(LibraryName, EntryPoint = "daedalus_bridge_engine_exchange_json", CallingConvention = CallingConvention.Cdecl)]
        internal static extern int EngineExchange(
            ulong engine,
            nint request,
            nuint requestLength,
            out ulong buffer,
            out nint data,
            out nuint length);

        [DllImport(LibraryName, EntryPoint = "daedalus_bridge_buffer_release", CallingConvention = CallingConvention.Cdecl)]
        internal static extern int BufferRelease(ulong buffer);
    }
}

internal sealed class EngineSafeHandle : SafeHandle
{
    internal EngineSafeHandle(ulong token) : base(nint.Zero, ownsHandle: true)
    {
        if (IntPtr.Size != sizeof(long))
        {
            throw new PlatformNotSupportedException("The managed host currently supports 64-bit processes only.");
        }
        if (token == 0 || token > long.MaxValue)
        {
            throw new ArgumentOutOfRangeException(nameof(token), "Native engine token is outside the supported SafeHandle range.");
        }
        SetHandle(new nint(checked((long)token)));
    }

    public override bool IsInvalid => handle == nint.Zero || handle == new nint(-1);

    protected override bool ReleaseHandle() => NativeBridge.DestroyEngine(checked((ulong)handle.ToInt64())) == 0;
}

internal sealed class BridgeBufferSafeHandle : SafeHandle
{
    internal BridgeBufferSafeHandle(ulong token) : base(nint.Zero, ownsHandle: true)
    {
        if (IntPtr.Size != sizeof(long))
        {
            throw new PlatformNotSupportedException("The managed host currently supports 64-bit processes only.");
        }
        if (token == 0 || token > long.MaxValue)
        {
            throw new ArgumentOutOfRangeException(nameof(token), "Native buffer token is outside the supported SafeHandle range.");
        }
        SetHandle(new nint(checked((long)token)));
    }

    public override bool IsInvalid => handle == nint.Zero || handle == new nint(-1);

    protected override bool ReleaseHandle() => NativeBridge.ReleaseBuffer(checked((ulong)handle.ToInt64())) == 0;
}
