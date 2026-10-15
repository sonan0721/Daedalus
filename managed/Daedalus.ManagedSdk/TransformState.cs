namespace Daedalus.ManagedSdk;

/// <summary>JSON-compatible transform data copied at the native boundary.</summary>
public sealed record TransformState(float[] Translation, float[] Rotation, float[] Scale)
{
    public static TransformState Identity => new(
        [0.0f, 0.0f, 0.0f],
        [0.0f, 0.0f, 0.0f, 1.0f],
        [1.0f, 1.0f, 1.0f]);

    public void Validate()
    {
        if (Translation.Length != 3 || Rotation.Length != 4 || Scale.Length != 3)
        {
            throw new ArgumentException("Transform requires translation[3], rotation[4], and scale[3].");
        }
        if (Translation.Concat(Rotation).Concat(Scale).Any(static value => !float.IsFinite(value)))
        {
            throw new ArgumentException("Transform values must all be finite.");
        }
    }

    public TransformState WithTranslationX(float value)
    {
        Validate();
        var translation = (float[])Translation.Clone();
        translation[0] = value;
        return this with { Translation = translation };
    }
}
