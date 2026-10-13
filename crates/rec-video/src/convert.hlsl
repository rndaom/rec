// FP16 scRGB capture -> planar Y'CbCr encoder input (P010 HDR10 when HDR10
// is defined, else tone-mapped NV12 BT.709). The why is in shader.rs.

cbuffer Params : register(b0)
{
    float2 dst_origin;   // letterbox rectangle in output luma pixels
    float2 dst_size;
    float2 src_size;     // captured content in texels
    float2 tex_size_inv; // 1 / capture texture size
    float scale;         // scRGB -> cd/m2 (HDR10) or -> fraction of SDR white
    float peak;          // HDR10: clip level in cd/m2
    float2 pad;
};

Texture2D<float4> capture : register(t0);
SamplerState bilinear : register(s0);

float4 vs_main(uint id : SV_VertexID) : SV_Position
{
    // One triangle that covers the whole viewport.
    float2 t = float2((id << 1) & 2, id & 2);
    return float4(t * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

#ifdef HDR10

// BT.2020 non-constant luminance; 10-bit codes in the top bits of 16.
static const float3 K = float3(0.2627, 0.6780, 0.0593);
static const float CB_DIV = 1.8814;
static const float CR_DIV = 1.4746;
static const float CODE_TO_UNORM = 64.0 / 65535.0;
static const float Y_BLACK = 64.0;
static const float Y_RANGE = 876.0;
static const float C_MID = 512.0;
static const float C_RANGE = 896.0;

static const float3x3 BT709_TO_BT2020 = {
    0.627403896, 0.329283039, 0.043313065,
    0.069097289, 0.919540395, 0.011362316,
    0.016391439, 0.088013308, 0.895595253,
};

float3 encode(float3 scrgb)
{
    float3 nits = clamp(mul(BT709_TO_BT2020, scrgb) * scale, 0.0, peak);
    // SMPTE ST 2084 inverse EOTF.
    float3 l = pow(nits / 10000.0, 0.1593017578125);
    return pow((0.8359375 + 18.8515625 * l) / (1.0 + 18.6875 * l), 78.84375);
}

#else

// BT.709, 8-bit.
static const float3 K = float3(0.2126, 0.7152, 0.0722);
static const float CB_DIV = 1.8556;
static const float CR_DIV = 1.5748;
static const float CODE_TO_UNORM = 1.0 / 255.0;
static const float Y_BLACK = 16.0;
static const float Y_RANGE = 219.0;
static const float C_MID = 128.0;
static const float C_RANGE = 224.0;

// Below this fraction of SDR white the picture is untouched; above it the
// brightest channel approaches 100% exponentially (slope 1 at the knee).
static const float KNEE = 0.85;

float3 encode(float3 scrgb)
{
    float3 x = max(scrgb * scale, 0.0);
    float m = max(x.r, max(x.g, x.b));
    if (m > KNEE)
        x *= (KNEE + (1.0 - KNEE) * (1.0 - exp((KNEE - m) / (1.0 - KNEE)))) / m;
    x = min(x, 1.0);
    // sRGB transfer, as the SDR path's desktop pixels already carry.
    return x <= 0.0031308 ? 12.92 * x : 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

#endif

// Non-linear R'G'B' at continuous output position p (luma pixels, centres
// at .5); black outside the letterbox rectangle.
float3 signal(float2 p)
{
    float2 t = (p - dst_origin) / dst_size;
    if (any(t < 0.0) || any(t > 1.0))
        return 0.0;
    // Stay half a texel inside the content: the texture can be larger.
    float2 texel = clamp(t * src_size, 0.5, src_size - 0.5);
    return encode(capture.SampleLevel(bilinear, texel * tex_size_inv, 0).rgb);
}

float ps_luma(float4 pos : SV_Position) : SV_Target
{
    float y = dot(signal(pos.xy), K);
    return round(Y_BLACK + Y_RANGE * y) * CODE_TO_UNORM;
}

float2 ps_chroma(float4 pos : SV_Position) : SV_Target
{
    // Chroma sample (i, j) is left-sited: on luma column 2i, halfway between
    // rows 2j and 2j + 1. Average [1 2 1]/4 across, [1 1]/2 down.
    float2 c = float2(2.0 * pos.x - 0.5, 2.0 * pos.y);
    float3 e = 0.0;
    [unroll] for (int r = 0; r < 2; r++)
    {
        float y = c.y + (r == 0 ? -0.5 : 0.5);
        e += 0.125 * signal(float2(c.x - 1.0, y))
           + 0.25 * signal(float2(c.x, y))
           + 0.125 * signal(float2(c.x + 1.0, y));
    }
    float y = dot(e, K);
    float2 cbcr = float2((e.b - y) / CB_DIV, (e.r - y) / CR_DIV);
    return round(C_MID + C_RANGE * cbcr) * CODE_TO_UNORM;
}
