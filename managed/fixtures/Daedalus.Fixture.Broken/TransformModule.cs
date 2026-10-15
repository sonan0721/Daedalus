using Daedalus.ManagedSdk;

namespace Daedalus.Fixture.Broken;

public sealed class TransformModule : IGameplayModule
{
    public string Id => "transform-fixture";

    public byte[] CaptureState() => [];

    public void RestoreState(ReadOnlyMemory<byte> state, IGameplayContext context)
    {
        if (state.IsEmpty)
        {
            return;
        }
        var entity = context.GetEntities().Single();
        var current = context.GetTransform(entity);
        context.RegisterCleanup(static () => { });
        context.SetTransform(entity, current.Transform.WithTranslationX(999.0f));
        throw new InvalidOperationException("fixture migration failure after staged mutation");
    }

    public void OnStart(IGameplayContext context)
    {
    }

    public void OnTick(IGameplayContext context)
    {
    }

    public void OnUnload()
    {
    }
}
