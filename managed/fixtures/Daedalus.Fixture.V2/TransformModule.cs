using System.Text.Json;
using Daedalus.ManagedSdk;

namespace Daedalus.Fixture.V2;

public sealed class TransformModule : IGameplayModule
{
    private int _ticks;
    private bool _failedOnce;

    public string Id => "transform-fixture";

    public byte[] CaptureState() => JsonSerializer.SerializeToUtf8Bytes(_ticks);

    public void RestoreState(ReadOnlyMemory<byte> state, IGameplayContext context)
    {
        _ticks = state.IsEmpty ? 0 : JsonSerializer.Deserialize<int>(state.Span);
    }

    public void OnStart(IGameplayContext context)
    {
        var entity = context.GetEntities().Single();
        var current = context.GetTransform(entity);
        // A no-op write exercises successful candidate staging while preserving native position.
        context.SetTransform(entity, current.Transform);
        context.RegisterCleanup(static () => { });
    }

    public void OnTick(IGameplayContext context)
    {
        if (!_failedOnce)
        {
            _failedOnce = true;
            context.RegisterCleanup(static () => { });
            throw new InvalidOperationException("fixture tick failure after cleanup registration");
        }
        var entity = context.GetEntities().Single();
        var current = context.GetTransform(entity);
        _ticks++;
        context.SetTransform(
            entity,
            current.Transform.WithTranslationX(current.Transform.Translation[0] + 10.0f + _ticks));
    }

    public void OnUnload()
    {
    }
}
