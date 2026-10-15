namespace Daedalus.ManagedSdk;

/// <summary>
/// A trusted, managed gameplay module. Each full replacement receives a new collectible
/// AssemblyLoadContext; persistent game state crosses generations only through these bytes.
/// </summary>
public interface IGameplayModule
{
    string Id { get; }

    byte[] CaptureState();

    void RestoreState(ReadOnlyMemory<byte> state, IGameplayContext context);

    void OnStart(IGameplayContext context);

    void OnTick(IGameplayContext context);

    void OnUnload();
}

/// <summary>
/// A callback-scoped facade over the Rust-owned world. Mutations are staged and committed as
/// one bounded transform command batch after the callback succeeds.
/// </summary>
public interface IGameplayContext
{
    IReadOnlyList<EntityHandle> GetEntities();

    EntityState GetTransform(EntityHandle entity);

    void SetTransform(EntityHandle entity, TransformState transform);

    void RegisterCleanup(Action cleanup);
}
