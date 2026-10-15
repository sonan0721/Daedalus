namespace Daedalus.ManagedSdk;

/// <summary>A Rust-owned entity identifier, scoped to one engine session and ECS generation.</summary>
public readonly record struct EntityHandle(string Session, uint Index, uint Generation);

/// <summary>A Rust-owned entity transform and its authoritative revision.</summary>
public sealed record EntityState(EntityHandle Entity, TransformState Transform, ulong Revision);
