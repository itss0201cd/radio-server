// pvq.rs — CELT pyramid vector quantiser (float build).
//
// `encode_pulses`/`decode_pulses` are the CWRS combinatorial coder from
// `celt/cwrs.c`; `op_pvq_search` is the greedy search used by the encoder.
// The `alg_unquant`/`decode_pulses` half is test-only, to check the bitstream.

#[cfg(test)]
use crate::range_coder::RangeDecoder;
use crate::range_coder::RangeEncoder;

pub const PVQ_U_DATA: [u32; 1488] = [
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31, 33, 35, 37, 39, 41, 43, 45, 47, 49,
    51, 53, 55, 57, 59, 61, 63, 65, 67, 69, 71, 73, 75, 77, 79, 81, 83, 85, 87, 89, 91, 93, 95, 97,
    99, 101, 103, 105, 107, 109, 111, 113, 115, 117, 119, 121, 123, 125, 127, 129, 131, 133, 135,
    137, 139, 141, 143, 145, 147, 149, 151, 153, 155, 157, 159, 161, 163, 165, 167, 169, 171, 173,
    175, 177, 179, 181, 183, 185, 187, 189, 191, 193, 195, 197, 199, 201, 203, 205, 207, 209, 211,
    213, 215, 217, 219, 221, 223, 225, 227, 229, 231, 233, 235, 237, 239, 241, 243, 245, 247, 249,
    251, 253, 255, 257, 259, 261, 263, 265, 267, 269, 271, 273, 275, 277, 279, 281, 283, 285, 287,
    289, 291, 293, 295, 297, 299, 301, 303, 305, 307, 309, 311, 313, 315, 317, 319, 321, 323, 325,
    327, 329, 331, 333, 335, 337, 339, 341, 343, 345, 347, 349, 351, 353, 355, 357, 359, 361, 363,
    365, 367, 369, 371, 373, 375, 377, 379, 381, 383, 385, 387, 389, 391, 393, 395, 397, 399, 401,
    403, 405, 407, 409, 411, 413, 415, 13, 25, 41, 61, 85, 113, 145, 181, 221, 265, 313, 365, 421,
    481, 545, 613, 685, 761, 841, 925, 1013, 1105, 1201, 1301, 1405, 1513, 1625, 1741, 1861, 1985,
    2113, 2245, 2381, 2521, 2665, 2813, 2965, 3121, 3281, 3445, 3613, 3785, 3961, 4141, 4325, 4513,
    4705, 4901, 5101, 5305, 5513, 5725, 5941, 6161, 6385, 6613, 6845, 7081, 7321, 7565, 7813, 8065,
    8321, 8581, 8845, 9113, 9385, 9661, 9941, 10225, 10513, 10805, 11101, 11401, 11705, 12013,
    12325, 12641, 12961, 13285, 13613, 13945, 14281, 14621, 14965, 15313, 15665, 16021, 16381,
    16745, 17113, 17485, 17861, 18241, 18625, 19013, 19405, 19801, 20201, 20605, 21013, 21425,
    21841, 22261, 22685, 23113, 23545, 23981, 24421, 24865, 25313, 25765, 26221, 26681, 27145,
    27613, 28085, 28561, 29041, 29525, 30013, 30505, 31001, 31501, 32005, 32513, 33025, 33541,
    34061, 34585, 35113, 35645, 36181, 36721, 37265, 37813, 38365, 38921, 39481, 40045, 40613,
    41185, 41761, 42341, 42925, 43513, 44105, 44701, 45301, 45905, 46513, 47125, 47741, 48361,
    48985, 49613, 50245, 50881, 51521, 52165, 52813, 53465, 54121, 54781, 55445, 56113, 56785,
    57461, 58141, 58825, 59513, 60205, 60901, 61601, 62305, 63013, 63725, 64441, 65161, 65885,
    66613, 67345, 68081, 68821, 69565, 70313, 71065, 71821, 72581, 73345, 74113, 74885, 75661,
    76441, 77225, 78013, 78805, 79601, 80401, 81205, 82013, 82825, 83641, 84461, 85285, 86113, 63,
    129, 231, 377, 575, 833, 1159, 1561, 2047, 2625, 3303, 4089, 4991, 6017, 7175, 8473, 9919,
    11521, 13287, 15225, 17343, 19649, 22151, 24857, 27775, 30913, 34279, 37881, 41727, 45825,
    50183, 54809, 59711, 64897, 70375, 76153, 82239, 88641, 95367, 102425, 109823, 117569, 125671,
    134137, 142975, 152193, 161799, 171801, 182207, 193025, 204263, 215929, 228031, 240577, 253575,
    267033, 280959, 295361, 310247, 325625, 341503, 357889, 374791, 392217, 410175, 428673, 447719,
    467321, 487487, 508225, 529543, 551449, 573951, 597057, 620775, 645113, 670079, 695681, 721927,
    748825, 776383, 804609, 833511, 863097, 893375, 924353, 956039, 988441, 1021567, 1055425,
    1090023, 1125369, 1161471, 1198337, 1235975, 1274393, 1313599, 1353601, 1394407, 1436025,
    1478463, 1521729, 1565831, 1610777, 1656575, 1703233, 1750759, 1799161, 1848447, 1898625,
    1949703, 2001689, 2054591, 2108417, 2163175, 2218873, 2275519, 2333121, 2391687, 2451225,
    2511743, 2573249, 2635751, 2699257, 2763775, 2829313, 2895879, 2963481, 3032127, 3101825,
    3172583, 3244409, 3317311, 3391297, 3466375, 3542553, 3619839, 3698241, 3777767, 3858425,
    3940223, 4023169, 4107271, 4192537, 4278975, 4366593, 4455399, 4545401, 4636607, 4729025,
    4822663, 4917529, 5013631, 5110977, 5209575, 5309433, 5410559, 5512961, 5616647, 5721625,
    5827903, 5935489, 6044391, 6154617, 6266175, 6379073, 6493319, 6608921, 6725887, 6844225,
    6963943, 7085049, 7207551, 7331457, 7456775, 7583513, 7711679, 7841281, 7972327, 8104825,
    8238783, 8374209, 8511111, 8649497, 8789375, 8930753, 9073639, 9218041, 9363967, 9511425,
    9660423, 9810969, 9963071, 10116737, 10271975, 10428793, 10587199, 10747201, 10908807,
    11072025, 11236863, 11403329, 11571431, 11741177, 11912575, 321, 681, 1289, 2241, 3649, 5641,
    8361, 11969, 16641, 22569, 29961, 39041, 50049, 63241, 78889, 97281, 118721, 143529, 172041,
    204609, 241601, 283401, 330409, 383041, 441729, 506921, 579081, 658689, 746241, 842249, 947241,
    1061761, 1186369, 1321641, 1468169, 1626561, 1797441, 1981449, 2179241, 2391489, 2618881,
    2862121, 3121929, 3399041, 3694209, 4008201, 4341801, 4695809, 5071041, 5468329, 5888521,
    6332481, 6801089, 7295241, 7815849, 8363841, 8940161, 9545769, 10181641, 10848769, 11548161,
    12280841, 13047849, 13850241, 14689089, 15565481, 16480521, 17435329, 18431041, 19468809,
    20549801, 21675201, 22846209, 24064041, 25329929, 26645121, 28010881, 29428489, 30899241,
    32424449, 34005441, 35643561, 37340169, 39096641, 40914369, 42794761, 44739241, 46749249,
    48826241, 50971689, 53187081, 55473921, 57833729, 60268041, 62778409, 65366401, 68033601,
    70781609, 73612041, 76526529, 79526721, 82614281, 85790889, 89058241, 92418049, 95872041,
    99421961, 103069569, 106816641, 110664969, 114616361, 118672641, 122835649, 127107241,
    131489289, 135983681, 140592321, 145317129, 150160041, 155123009, 160208001, 165417001,
    170752009, 176215041, 181808129, 187533321, 193392681, 199388289, 205522241, 211796649,
    218213641, 224775361, 231483969, 238341641, 245350569, 252512961, 259831041, 267307049,
    274943241, 282741889, 290705281, 298835721, 307135529, 315607041, 324252609, 333074601,
    342075401, 351257409, 360623041, 370174729, 379914921, 389846081, 399970689, 410291241,
    420810249, 431530241, 442453761, 453583369, 464921641, 476471169, 488234561, 500214441,
    512413449, 524834241, 537479489, 550351881, 563454121, 576788929, 590359041, 604167209,
    618216201, 632508801, 647047809, 661836041, 676876329, 692171521, 707724481, 723538089,
    739615241, 755958849, 772571841, 789457161, 806617769, 824056641, 841776769, 859781161,
    878072841, 896654849, 915530241, 934702089, 954173481, 973947521, 994027329, 1014416041,
    1035116809, 1056132801, 1077467201, 1099123209, 1121104041, 1143412929, 1166053121, 1189027881,
    1212340489, 1235994241, 1683, 3653, 7183, 13073, 22363, 36365, 56695, 85305, 124515, 177045,
    246047, 335137, 448427, 590557, 766727, 982729, 1244979, 1560549, 1937199, 2383409, 2908411,
    3522221, 4235671, 5060441, 6009091, 7095093, 8332863, 9737793, 11326283, 13115773, 15124775,
    17372905, 19880915, 22670725, 25765455, 29189457, 32968347, 37129037, 41699767, 46710137,
    52191139, 58175189, 64696159, 71789409, 79491819, 87841821, 96879431, 106646281, 117185651,
    128542501, 140763503, 153897073, 167993403, 183104493, 199284183, 216588185, 235074115,
    254801525, 275831935, 298228865, 322057867, 347386557, 374284647, 402823977, 433078547,
    465124549, 499040399, 534906769, 572806619, 612825229, 655050231, 699571641, 746481891,
    795875861, 847850911, 902506913, 959946283, 1020274013, 1083597703, 1150027593, 1219676595,
    1292660325, 1369097135, 1449108145, 1532817275, 1620351277, 1711839767, 1807415257, 1907213187,
    2011371957, 2120032959, 2233340609, 2351442379, 2474488829, 2602633639, 2736033641, 2874848851,
    3019242501, 3169381071, 3325434321, 3487575323, 3655980493, 3830829623, 4012305913, 8989,
    19825, 40081, 75517, 134245, 227305, 369305, 579125, 880685, 1303777, 1884961, 2668525,
    3707509, 5064793, 6814249, 9041957, 11847485, 15345233, 19665841, 24957661, 31388293, 39146185,
    48442297, 59511829, 72616013, 88043969, 106114625, 127178701, 151620757, 179861305, 212358985,
    249612805, 292164445, 340600625, 395555537, 457713341, 527810725, 606639529, 695049433,
    793950709, 904317037, 1027188385, 1163673953, 1314955181, 1482288821, 1667010073, 1870535785,
    2094367717, 2340095869, 2609401873, 2904062449, 3225952925, 3577050821, 3959439497, 48639,
    108545, 224143, 433905, 795455, 1392065, 2340495, 3800305, 5984767, 9173505, 13726991,
    20103025, 28875327, 40754369, 56610575, 77500017, 104692735, 139703809, 184327311, 240673265,
    311207743, 398796225, 506750351, 638878193, 799538175, 993696769, 1226990095, 1505789553,
    1837271615, 2229491905, 2691463695, 3233240945, 3866006015, 265729, 598417, 1256465, 2485825,
    4673345, 8405905, 14546705, 24331777, 39490049, 62390545, 96220561, 145198913, 214828609,
    312193553, 446304145, 628496897, 872893441, 1196924561, 1621925137, 2173806145, 2883810113,
    1462563, 3317445, 7059735, 14218905, 27298155, 50250765, 89129247, 152951073, 254831667,
    413442773, 654862247, 1014889769, 1541911931, 2300409629, 3375210671, 8097453, 18474633,
    39753273, 81270333, 158819253, 298199265, 540279585, 948062325, 1616336765, 2684641785,
    45046719, 103274625, 224298231, 464387817, 921406335, 1759885185, 3248227095, 251595969,
    579168825, 1267854873, 2653649025, 1409933619,
];

pub const PVQ_U_ROW: [usize; 15] = [
    0, 208, 415, 621, 826, 1030, 1233, 1336, 1389, 1421, 1441, 1455, 1464, 1470, 1473,
];

fn pvq_u(n: usize, k: usize) -> u32 {
    let (a, b) = if n < k { (n, k) } else { (k, n) };
    PVQ_U_DATA[PVQ_U_ROW[a] + b]
}

fn pvq_v(n: usize, k: usize) -> u32 {
    pvq_u(n, k) + pvq_u(n, k + 1)
}

fn icwrs(n: usize, y: &[i32]) -> u32 {
    let mut j = n - 1;
    let mut i = (y[j] < 0) as u32;
    let mut k = y[j].unsigned_abs() as usize;
    loop {
        j -= 1;
        i += pvq_u(n - j, k);
        k += y[j].unsigned_abs() as usize;
        if y[j] < 0 {
            i += pvq_u(n - j, k + 1);
        }
        if j == 0 {
            break;
        }
    }
    i
}

pub fn encode_pulses(enc: &mut RangeEncoder, y: &[i32], n: usize, k: usize) {
    enc.encode_uint(icwrs(n, y), pvq_v(n, k));
}

#[cfg(test)]
fn cwrsi(n: usize, k_in: usize, i_in: u32, y: &mut [i32]) -> f32 {
    let mut n = n;
    let mut k = k_in;
    let mut i = i_in;
    let mut yy = 0.0f32;
    let mut pos = 0;
    while n > 2 {
        if k >= n {
            let row = PVQ_U_ROW[n];
            let mut p = PVQ_U_DATA[row + k + 1];
            let s = if i >= p { -1 } else { 0 };
            if s != 0 {
                i -= p;
            }
            let k0 = k;
            let q = PVQ_U_DATA[row + n];
            if q > i {
                k = n;
                loop {
                    k -= 1;
                    p = pvq_u(k, n);
                    if p <= i {
                        break;
                    }
                }
            } else {
                loop {
                    p = PVQ_U_DATA[row + k];
                    if p <= i {
                        break;
                    }
                    k -= 1;
                }
            }
            i -= p;
            let val = (k0 as i32 - k as i32 + s) ^ s;
            y[pos] = val;
            pos += 1;
            yy += (val * val) as f32;
        } else {
            let p = PVQ_U_DATA[PVQ_U_ROW[k] + n];
            let q = PVQ_U_DATA[PVQ_U_ROW[k + 1] + n];
            if p <= i && i < q {
                i -= p;
                y[pos] = 0;
                pos += 1;
            } else {
                let s = if i >= q { -1 } else { 0 };
                if s != 0 {
                    i -= q;
                }
                let k0 = k;
                loop {
                    k -= 1;
                    if PVQ_U_DATA[PVQ_U_ROW[k] + n] <= i {
                        break;
                    }
                }
                i -= PVQ_U_DATA[PVQ_U_ROW[k] + n];
                let val = (k0 as i32 - k as i32 + s) ^ s;
                y[pos] = val;
                pos += 1;
                yy += (val * val) as f32;
            }
        }
        n -= 1;
    }
    let p = 2 * k + 1;
    let s = if i >= p as u32 { -1 } else { 0 };
    if s != 0 {
        i -= p as u32;
    }
    let k0 = k;
    k = ((i + 1) >> 1) as usize;
    if k != 0 {
        i -= (2 * k - 1) as u32;
    }
    let val = (k0 as i32 - k as i32 + s) ^ s;
    y[pos] = val;
    pos += 1;
    yy += (val * val) as f32;

    let s = if i != 0 { -1 } else { 0 };
    let val = (k as i32 + s) ^ s;
    y[pos] = val;
    yy += (val * val) as f32;
    yy
}

#[cfg(test)]
pub fn decode_pulses(dec: &mut RangeDecoder, y: &mut [i32], n: usize, k: usize) -> f32 {
    let i = dec.decode_uint(pvq_v(n, k));
    cwrsi(n, k, i, y)
}

const PI: f32 = 3.141592653;
const EPSILON: f32 = 1e-15;

fn celt_cos_norm(x: f32) -> f32 {
    ((0.5f32 * PI * x) as f64).cos() as f32
}

fn exp_rotation1(x: &mut [f32], len: usize, stride: usize, c: f32, s: f32) {
    let ms = -s;
    for i in 0..len - stride {
        let x1 = x[i];
        let x2 = x[i + stride];
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 + ms * x2;
    }
    if len >= 2 * stride + 1 {
        for i in (0..=len - 2 * stride - 1).rev() {
            let x1 = x[i];
            let x2 = x[i + stride];
            x[i + stride] = c * x2 + s * x1;
            x[i] = c * x1 + ms * x2;
        }
    }
}

pub fn exp_rotation(x: &mut [f32], len: usize, dir: i32, stride: usize, k: usize, spread: i32) {
    if 2 * k >= len || spread == 0 {
        return;
    }
    let factor = [15, 10, 5][(spread - 1) as usize];
    let gain = len as f32 / (len + factor * k) as f32;
    let theta = 0.5 * gain * gain;
    let c = celt_cos_norm(theta);
    let s = celt_cos_norm(1.0 - theta);
    let mut stride2 = 0usize;
    if len >= 8 * stride {
        stride2 = 1;
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let len = len / stride;
    for i in 0..stride {
        let seg = &mut x[i * len..(i + 1) * len];
        if dir < 0 {
            if stride2 != 0 {
                exp_rotation1(seg, len, stride2, s, c);
            }
            exp_rotation1(seg, len, 1, c, s);
        } else {
            exp_rotation1(seg, len, 1, c, -s);
            if stride2 != 0 {
                exp_rotation1(seg, len, stride2, s, -c);
            }
        }
    }
}

pub fn op_pvq_search(x: &mut [f32], iy: &mut [i32], k: usize, n: usize) -> f32 {
    debug_assert!(n <= 176 && n <= x.len() && n <= iy.len());
    // Truncating to `n` lets LLVM prove every index below is in range.
    let x = &mut x[..n];
    let iy = &mut iy[..n];
    let mut signx = [0i32; 176];
    let signx = &mut signx[..n];
    // `2.0 * iy[j]` is exact for these small integers, so caching it as f32
    // removes a conversion and add from every candidate of every pulse.
    let mut iy2 = [0.0f32; 176];
    let iy2 = &mut iy2[..n];
    let mut sum = 0.0f32;
    for j in 0..n {
        signx[j] = (x[j] < 0.0) as i32;
        x[j] = x[j].abs();
    }
    let mut xy = 0.0f32;
    let mut yy = 0.0f32;
    let mut pulses_left = k as i32;

    if k > n / 2 {
        for j in 0..n {
            sum += x[j];
        }
        if !(sum > EPSILON && sum < 64.0) {
            x[0] = 1.0;
            x[1..].fill(0.0);
            sum = 1.0;
        }
        let rcp = (k as f32 + 0.8) * (1.0 / sum);
        for j in 0..n {
            iy[j] = (rcp * x[j]).floor() as i32;
            let iyf = iy[j] as f32;
            iy2[j] = 2.0 * iyf;
            yy += iyf * iyf;
            xy += x[j] * iyf;
            pulses_left -= iy[j];
        }
    } else {
        iy.fill(0);
        iy2.fill(0.0);
    }
    // proofs/Pvq.lean: `shortcut_dead` shows the floor pass leaves at most
    // n - 1 pulses, so the old `pulses_left > n + 3` shortcut was unreachable.
    debug_assert!(pulses_left >= 0);
    debug_assert!(pulses_left < n as i32 + 3);

    for _ in 0..pulses_left {
        yy += 1.0;
        let mut best_id = 0usize;
        let rxy = xy + x[0];
        let ryy = yy + iy2[0];
        let mut best_num = rxy * rxy;
        let mut best_den = ryy;
        // Four candidates per iteration so the independent quotients can be
        // evaluated in parallel; the comparisons still run in index order and
        // each literal keeps its exact f32 operations.
        let mut j = 1usize;
        while j + 4 <= n {
            let r0 = xy + x[j];
            let r1 = xy + x[j + 1];
            let r2 = xy + x[j + 2];
            let r3 = xy + x[j + 3];
            let d0 = yy + iy2[j];
            let d1 = yy + iy2[j + 1];
            let d2 = yy + iy2[j + 2];
            let d3 = yy + iy2[j + 3];
            let n0 = r0 * r0;
            let n1 = r1 * r1;
            let n2 = r2 * r2;
            let n3 = r3 * r3;
            if best_den * n0 > d0 * best_num {
                best_den = d0;
                best_num = n0;
                best_id = j;
            }
            if best_den * n1 > d1 * best_num {
                best_den = d1;
                best_num = n1;
                best_id = j + 1;
            }
            if best_den * n2 > d2 * best_num {
                best_den = d2;
                best_num = n2;
                best_id = j + 2;
            }
            if best_den * n3 > d3 * best_num {
                best_den = d3;
                best_num = n3;
                best_id = j + 3;
            }
            j += 4;
        }
        while j < n {
            let rxy = xy + x[j];
            let ryy = yy + iy2[j];
            let rxy2 = rxy * rxy;
            if best_den * rxy2 > ryy * best_num {
                best_den = ryy;
                best_num = rxy2;
                best_id = j;
            }
            j += 1;
        }
        xy += x[best_id];
        yy += iy2[best_id];
        iy2[best_id] += 2.0;
        iy[best_id] += 1;
    }

    for j in 0..n {
        iy[j] = (iy[j] ^ -signx[j]) + signx[j];
    }
    yy
}

fn normalise_residual(iy: &[i32], x: &mut [f32], n: usize, ryy: f32, gain: f32) {
    let g = gain * (1.0f32 / ((ryy as f64).sqrt() as f32));
    for (x, &iy) in x[..n].iter_mut().zip(&iy[..n]) {
        *x = g * iy as f32;
    }
}

pub fn renormalise_vector(x: &mut [f32], n: usize, gain: f32) {
    let mut e = EPSILON;
    for value in x.iter().take(n) {
        e += value * value;
    }
    let g = gain * (1.0f32 / ((e as f64).sqrt() as f32));
    for value in x.iter_mut().take(n) {
        *value *= g;
    }
}

fn extract_collapse_mask(iy: &[i32], n: usize, b: usize) -> u32 {
    if b <= 1 {
        return 1;
    }
    let n0 = n / b;
    let mut mask = 0u32;
    for i in 0..b {
        let mut tmp = 0i32;
        for j in 0..n0 {
            tmp |= iy[i * n0 + j];
        }
        mask |= ((tmp != 0) as u32) << i;
    }
    mask
}

pub struct Quant {
    pub n: usize,
    pub k: usize,
    pub spread: i32,
    pub b: usize,
    pub gain: f32,
    pub resynth: bool,
}

pub fn alg_quant(enc: &mut RangeEncoder, x: &mut [f32], q: Quant) -> u32 {
    let Quant {
        n,
        k,
        spread,
        b,
        gain,
        resynth,
    } = q;
    debug_assert!(n <= 176 && n <= x.len());
    let x = &mut x[..n];
    let mut iy = [0i32; 179];
    exp_rotation(x, n, 1, b, k, spread);
    let yy = op_pvq_search(x, &mut iy, k, n);
    encode_pulses(enc, &iy[..n], n, k);
    if resynth {
        normalise_residual(&iy, x, n, yy, gain);
        exp_rotation(x, n, -1, b, k, spread);
    }
    extract_collapse_mask(&iy, n, b)
}

#[cfg(test)]
pub fn alg_unquant(
    dec: &mut RangeDecoder,
    x: &mut [f32],
    n: usize,
    k: usize,
    spread: i32,
    b: usize,
    gain: f32,
) -> u32 {
    let mut iy = vec![0i32; n];
    let ryy = decode_pulses(dec, &mut iy, n, k);
    normalise_residual(&iy, x, n, ryy, gain);
    exp_rotation(x, n, -1, b, k, spread);
    extract_collapse_mask(&iy, n, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn random_pulses(rng: &mut Rng, n: usize, k: usize) -> Vec<i32> {
        let mut y = vec![0i32; n];
        for _ in 0..k {
            let j = (rng.next() % n as u64) as usize;
            y[j] += 1;
        }
        for v in &mut y {
            if *v != 0 && rng.next() & 1 == 1 {
                *v = -*v;
            }
        }
        y
    }

    /// `proofs/Pvq.lean::shortcut_dead`: the floor pass leaves at most `n - 1`
    /// pulses, so the old `pulses_left > n + 3` shortcut in `op_pvq_search`
    /// could never run. This walks the whole `(n, k)` domain the encoder emits
    /// and repeats the exact f32 arithmetic, so any change that makes the
    /// shortcut reachable again fails here.
    #[test]
    fn floor_pass_keeps_pulses_left_below_shortcut() {
        let mut rng = Rng(0x5EED_F00D_1234_5678);
        for n in 2..=176usize {
            for k in 0..=128usize {
                if k <= n / 2 {
                    assert!(k <= n - 1, "low branch: n={n} k={k}");
                    continue;
                }
                for _ in 0..4 {
                    let mut x: Vec<f32> = (0..n)
                        .map(|_| {
                            let f = (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
                            ((f * 2.0 - 1.0) as f32).abs()
                        })
                        .collect();
                    let mut sum = 0.0f32;
                    for v in &x {
                        sum += *v;
                    }
                    if !(sum > EPSILON && sum < 64.0) {
                        x[0] = 1.0;
                        for v in x.iter_mut().skip(1) {
                            *v = 0.0;
                        }
                        sum = 1.0;
                    }
                    let rcp = (k as f32 + 0.8) * (1.0 / sum);
                    let used: i32 = x.iter().map(|v| (rcp * v).floor() as i32).sum();
                    let pulses_left = k as i32 - used;
                    assert!(
                        pulses_left < n as i32 + 3,
                        "floor pass: n={n} k={k} left={pulses_left}"
                    );
                }
            }
        }
    }

    #[test]
    fn cwrs_round_trip() {
        let pairs: &[(usize, usize)] = &[
            (2, 1),
            (2, 5),
            (3, 1),
            (3, 7),
            (4, 2),
            (8, 3),
            (16, 4),
            (32, 5),
            (60, 3),
            (96, 2),
            (176, 1),
            (176, 2),
        ];
        for &(n, k) in pairs {
            let mut buf = vec![0u8; 4096];
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((n * 31 + k) as u64));
            let mut ys = Vec::new();
            {
                let mut enc = RangeEncoder::new(&mut buf);
                for _ in 0..16 {
                    let y = random_pulses(&mut rng, n, k);
                    encode_pulses(&mut enc, &y, n, k);
                    ys.push(y);
                }
                enc.done();
                assert!(!enc.error());
            }
            let mut dec = RangeDecoder::new(&buf);
            for y in ys {
                let want = y.iter().map(|&v| (v * v) as f32).sum::<f32>();
                let mut got = vec![0i32; n];
                let yy = decode_pulses(&mut dec, &mut got, n, k);
                assert_eq!(got, y, "n={n} k={k}");
                assert_eq!(yy.to_bits(), want.to_bits(), "n={n} k={k} yy");
            }
        }
    }

    #[test]
    fn quant_unquant_is_bit_exact() {
        let cases: &[(usize, usize, i32, usize)] =
            &[(8, 3, 2, 1), (16, 5, 1, 2), (32, 7, 3, 1), (60, 4, 2, 1)];
        for &(n, k, spread, b) in cases {
            let mut rng = Rng(0x1234_5678 ^ ((n * 17 + k) as u64));
            let source: Vec<f32> = (0..n)
                .map(|_| ((rng.next() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32)
                .collect();
            let mut enc_x = source.clone();
            let mut dec_x = vec![0.0f32; n];
            let mut buf = vec![0u8; 4096];
            let enc_mask;
            {
                let mut enc = RangeEncoder::new(&mut buf);
                enc_mask = alg_quant(
                    &mut enc,
                    &mut enc_x,
                    Quant {
                        n,
                        k,
                        spread,
                        b,
                        gain: 0.75,
                        resynth: true,
                    },
                );
                enc.done();
                assert!(!enc.error());
            }
            let mut dec = RangeDecoder::new(&buf);
            let dec_mask = alg_unquant(&mut dec, &mut dec_x, n, k, spread, b, 0.75);
            assert_eq!(dec_mask, enc_mask, "n={n} k={k}");
            for i in 0..n {
                assert_eq!(dec_x[i].to_bits(), enc_x[i].to_bits(), "n={n} k={k} i={i}");
            }
        }
    }

    pub struct RefCase {
        pub n: usize,
        pub k: usize,
        pub spread: i32,
        pub b: usize,
        pub seed: u32,
        pub rot_hash: u64,
        pub hash: u64,
        pub mask: u32,
        pub x: &'static [u32],
    }

    pub const REF_CASES: &[RefCase] = &[
        RefCase {
            n: 8,
            k: 3,
            spread: 2,
            b: 1,
            seed: 0x12345678,
            rot_hash: 0x9b3245bc37867d7d,
            hash: 0x61a0100a4ce5a5f9,
            mask: 1,
            x: &[
                0xbe939f65, 0xbc2a3929, 0x3c23ccf1, 0xbc0d69a3, 0x3e8900a8, 0x3e9d9c76, 0x3c359150,
                0xbc2435b6,
            ],
        },
        RefCase {
            n: 16,
            k: 5,
            spread: 1,
            b: 2,
            seed: 0x9abcdef0,
            rot_hash: 0x80489b986f2fa30d,
            hash: 0xd1dbfb1bbccc57dd,
            mask: 3,
            x: &[
                0x3c35e8ed, 0xbb9bda3a, 0xbc35ac1a, 0x3e69b72b, 0xbe59c015, 0x3a0a6b68, 0xbe6a72c7,
                0xb90bb5a0, 0xbbb61803, 0x3bad4f8e, 0x3986c30e, 0xbbad6a61, 0x3e64d77f, 0xb5d66000,
                0x3e64e7e4, 0x35d6b000,
            ],
        },
        RefCase {
            n: 32,
            k: 7,
            spread: 3,
            b: 1,
            seed: 0x0badc0de,
            rot_hash: 0x9dff1eef20519762,
            hash: 0x9aaffa43080506c7,
            mask: 1,
            x: &[
                0xbce69488, 0xbb6b7870, 0xbc40a628, 0xbd97f44d, 0x3e34a3bc, 0x3d8892ac, 0xbe2da26e,
                0xbd9c42c2, 0x3cb88309, 0x3c3ea600, 0xbae7d794, 0x3c04d715, 0x3c41b340, 0xbbdc84f7,
                0x3cdf624c, 0xbb2e8da2, 0x3b0e88a3, 0x3e34127c, 0x3d8ff28c, 0xbb063d56, 0x3d471673,
                0xbd78c981, 0x3e6b5dfa, 0xbddb1f62, 0xbd100f15, 0xbd8b997e, 0x3e28b5eb, 0xba3baf40,
                0x3e2c245e, 0x3d5094a6, 0x3d25a686, 0x3bd766e8,
            ],
        },
        RefCase {
            n: 60,
            k: 4,
            spread: 2,
            b: 1,
            seed: 0xdeadbeef,
            rot_hash: 0x1678c662f5f6560f,
            hash: 0xe1d765546021ffa7,
            mask: 1,
            x: &[
                0xbd1cd7fb, 0xbcefa05b, 0x3b852df2, 0xbbd838f8, 0x3d16b3de, 0xbd529bf5, 0xbd40d69c,
                0x3ce07db8, 0xbda103df, 0x3dbcaf8b, 0xbe418d04, 0xbdf59e4a, 0xbd3b7c21, 0x3c14559a,
                0xbb001368, 0x3d1aecf0, 0xbd48885e, 0xbd6e7cc6, 0x3cb15610, 0x3c775a8c, 0x3c7f3204,
                0xbc57825a, 0xbc9152bd, 0x3cb64411, 0xbd1df86a, 0xbcc8cea9, 0xbb679a06, 0xbd0f997c,
                0x3dd85074, 0xbe222bf0, 0xbdf02ea0, 0x3b62f0d8, 0xbd613131, 0xbd6fdf98, 0x3ca7b790,
                0x3ca77549, 0xbb322705, 0x3c66e8eb, 0x3ab4c4ac, 0x3ccbb8a6, 0xbd0b99f9, 0xbc8aef26,
                0xbc6c9687, 0xbb5b3f09, 0xbc60855e, 0x3c9ba47d, 0xba288cf8, 0x3d348849, 0xbd9c68d2,
                0x3d85b345, 0xbe211975, 0xbdbb9a8d, 0xbd730456, 0x3d2f0f56, 0x3b6914a4, 0x3dbdf302,
                0xbdeafffd, 0xbe19dc94, 0x3d936135, 0x3ceb1850,
            ],
        },
        RefCase {
            n: 176,
            k: 2,
            spread: 1,
            b: 1,
            seed: 0x31415926,
            rot_hash: 0xea0b140cbf73b577,
            hash: 0xd76f33a07ee173ea,
            mask: 1,
            x: &[
                0x3c1b0974, 0xbc7021cf, 0x3cdd5bb4, 0xbd4c0d5a, 0x3c1f11a2, 0x3d35f9d6, 0x3d0b67ca,
                0x3913c340, 0x3d0aa783, 0xb9545740, 0xbcba6968, 0xbc6a3f42, 0xbb88070d, 0xbc11bed5,
                0x3bf344fc, 0xbc98029b, 0x3d014f13, 0xbbd8713a, 0xbcec4114, 0xbcb51c42, 0xb8967300,
                0xbcb4587c, 0x39bb6c60, 0x3c69429e, 0x3c24a458, 0x3a982f00, 0x3c0be382, 0xbc23012e,
                0x3cafff6a, 0xbd1b420e, 0x3bfbd174, 0x3d0c36b6, 0x3cd72666, 0x386f6000, 0x3cd68aa7,
                0xb9e375e0, 0xbc8adcf7, 0xbc4403ed, 0xbab551ee, 0xbc2685de, 0x3c420512, 0xbcd17e69,
                0x3d38ce0d, 0xbc15df3e, 0xbd26e5e9, 0xbd000c3c, 0xb88e7c80, 0xbcff5f0e, 0x3a075fa0,
                0x3ca54a54, 0x3c6951b7, 0x3ad7d3de, 0x3c4636c9, 0xbc66f19d, 0x3cf95cd8, 0xbd5bf9a8,
                0x3c3264c0, 0x3d46a938, 0x3d186a82, 0x38aa3b80, 0x3d17fbc0, 0xba20dea0, 0xbcc4c325,
                0xbc8ad53c, 0xbb00de4a, 0xbc6bbe77, 0x3c89452c, 0xbd143f05, 0x3d82c4d2, 0xbc521f36,
                0xbd6d7e16, 0xbd3388d2, 0xba781b20, 0xbd2e7d74, 0xbb0d6f00, 0x3d0aead5, 0x3c29a59d,
                0x3ca775ad, 0xbc85148f, 0x3d2a5060, 0xbd90143a, 0x3e08518d, 0xbc8919f4, 0xbe0fa2e2,
                0xbd7a11d9, 0xbd792582, 0x3c94414a, 0xbc8ade05, 0xbd42e258, 0xbcb80744, 0xbc964e07,
                0x3ab8d8bc, 0xbca5567c, 0x3ce2b218, 0xbd662922, 0x3b82e6e0, 0x3d843a6a, 0x3c897148,
                0x3d2f08bd, 0xbd1f09e5, 0x3c55021e, 0x3d476b5a, 0x3cf4faa0, 0x3c2020f4, 0x3c8ccbf7,
                0xbc4e4a1e, 0x3d098188, 0xbd667fdd, 0x3c650e7c, 0x3d3daea2, 0x3d3e9e6a, 0xbc696d8c,
                0x3d854701, 0xbb5eae68, 0xbd607e0c, 0xbce22dfa, 0xbc9aac3c, 0xbb4750f9, 0xbc7481a5,
                0x3c862a49, 0xbd146aa4, 0xba48a7c0, 0x3d465502, 0x3a0862b0, 0x3d438f22, 0xbd7c562c,
                0x3c689bb0, 0x3d8b0658, 0x3d2219c4, 0x3c863d00, 0x3c9586b6, 0xbbf151d2, 0x3ce40785,
                0xbd328640, 0x3c6972fa, 0x3cf07473, 0x3d3ddf61, 0xbcfdae71, 0x3db175f9, 0xbc0920dc,
                0xbda0ee93, 0xbd29db4a, 0xbcc9aeac, 0xbc1a8e3e, 0xbc4a4084, 0x3bd93df6, 0xbcabcbea,
                0xbbbc9643, 0x3d1c3a9f, 0xbc80b8fa, 0x3d6ffa58, 0xbdbc5064, 0x3c8c5058, 0x3dc361fd,
                0x3d5d94ec, 0x3cccdf76, 0x3cb09390, 0xbb47269c, 0x3cd0f816, 0xbd1467cf, 0x3c85187e,
                0x3c834a6d, 0x3d542cb4, 0xbd527162, 0x3df30138, 0xbc6576d0, 0xbde6b8fa, 0xbd726827,
                0xbd12048d, 0xbc519da4, 0xbc9ba801, 0x3c43794c, 0xbd0d6ad5, 0x395ec780, 0x3d1f5af2,
                0x3c5fa608,
            ],
        },
        RefCase {
            n: 4,
            k: 8,
            spread: 2,
            b: 1,
            seed: 0x27182818,
            rot_hash: 0x16e58d8c9ea3198c,
            hash: 0x3beab9fb608ca425,
            mask: 1,
            x: &[0x3eb504f4, 0x3e715bf0, 0x3df15bf0, 0xbe715bf0],
        },
        RefCase {
            n: 3,
            k: 7,
            spread: 0,
            b: 1,
            seed: 0xfeedface,
            rot_hash: 0x0c541a9979ba60ad,
            hash: 0x2a93a183bad0e666,
            mask: 1,
            x: &[0xbeba4472, 0xbe785b43, 0xbe785b43],
        },
    ];

    fn hash_bits(x: &[f32]) -> u64 {
        let mut h = 14695981039346656037u64;
        for &v in x {
            h ^= v.to_bits() as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    fn hash_bytes(data: &[u8]) -> u64 {
        let mut h = 14695981039346656037u64;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    fn source(seed: u32, n: usize) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (((s >> 16) % 2001) as i32 - 1000) as f32 * 0.001
            })
            .collect()
    }

    #[test]
    fn matches_reference_pvq() {
        for case in REF_CASES {
            let mut x = source(case.seed, case.n);
            let mut xr = x.clone();
            exp_rotation(&mut xr, case.n, 1, case.b, case.k, case.spread);
            assert_eq!(
                hash_bits(&xr),
                case.rot_hash,
                "rot n={} k={}",
                case.n,
                case.k
            );

            let mut buf = vec![0u8; 4096];
            let mask;
            {
                let mut enc = RangeEncoder::new(&mut buf);
                mask = alg_quant(
                    &mut enc,
                    &mut x,
                    Quant {
                        n: case.n,
                        k: case.k,
                        spread: case.spread,
                        b: case.b,
                        gain: 0.5,
                        resynth: true,
                    },
                );
                enc.done();
                assert!(!enc.error());
            }
            assert_eq!(
                hash_bytes(&buf),
                case.hash,
                "hash n={} k={}",
                case.n,
                case.k
            );
            assert_eq!(mask, case.mask);

            let mut xd = vec![0.0f32; case.n];
            let mut dec = RangeDecoder::new(&buf);
            let mask2 = alg_unquant(&mut dec, &mut xd, case.n, case.k, case.spread, case.b, 0.5);
            assert_eq!(mask2, case.mask);
            for i in 0..case.n {
                assert_eq!(
                    x[i].to_bits(),
                    case.x[i],
                    "x n={} k={} i={}",
                    case.n,
                    case.k,
                    i
                );
                assert_eq!(xd[i].to_bits(), x[i].to_bits());
            }
        }
    }

    #[test]
    fn renormalise_sets_unit_norm() {
        let mut x = vec![0.3f32, -0.7, 0.2, 0.9, -0.1, 0.4, 0.0, 0.5];
        renormalise_vector(&mut x, 8, 0.7);
        let e: f32 = x.iter().map(|v| v * v).sum();
        assert!((e - 0.49).abs() < 1e-5, "energy {e}");
    }

    #[test]
    fn rotation_preserves_norm() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_1234);
        let n = 32;
        let mut x: Vec<f32> = (0..n)
            .map(|_| ((rng.next() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32)
            .collect();
        let before: f32 = x.iter().map(|v| v * v).sum();
        exp_rotation(&mut x, n, 1, 1, 4, 2);
        let after: f32 = x.iter().map(|v| v * v).sum();
        assert!((before - after).abs() < 1e-4, "{before} vs {after}");
    }
}
