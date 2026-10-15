using System.Text.Json;
using Daedalus.ManagedSdk;

namespace Daedalus.Fixture.V1;

public sealed class TransformModule : IGameplayModule
{
    private int _ticks;

    public string Id => "transform-fixture";

    public byte[] CaptureState() => JsonSerializer.SerializeToUtf8Bytes(_ticks);

    public void RestoreState(ReadOnlyMemory<byte> state, IGameplayContext context)
    {
        _ticks = state.IsEmpty ? 0 : JsonSerializer.Deserialize<int>(state.Span);
    }

    public void OnStart(IGameplayContext context)
    {
        context.RegisterCleanup(static () => { });
    }

    public void OnTick(IGameplayContext context)
    {
        var entity = context.GetEntities().Single();
        var current = context.GetTransform(entity);
        _ticks++;
        context.SetTransform(entity, current.Transform.WithTranslationX(current.Transform.Translation[0] + 0.25f));
        var staged = context.GetTransform(entity);
        if (staged.Transform.Translation[0] != current.Transform.Translation[0] + 0.25f)
        {
            throw new InvalidOperationException("staged transform reads must observe the callback's prior write");
        }
        context.SetTransform(entity, staged.Transform.WithTranslationX(staged.Transform.Translation[0] + 0.75f));
    }

    public void OnUnload()
    {
    }
}
