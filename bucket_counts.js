const bucket_items_50 = [
    0,
    615,
    205,
    290,
    180,
    169,
    132,
    94,
    82,
    91,
    79,
    55,
    43,
    59,
    61,
    58,
    44,
    117,
    115,
    85,
    73,
    68,
    92,
    109,
    105,
    327,
    244,
    39,
    10,
    2,
    0,
    2,
    1,
    0,
    0,
    1,
]
const bucket_items_10 = [
    0,
    7567,
    1512,
    1095,
    2417,
    3104,
    2367,
    137,
    19,
    13,
    1,
]


const size_counts = [
    0,
    5496,
    24211,
    7895,
    899,
    329,
    258,
    240,
    232,
    194,
    204,
    182,
    158,
    140,
    146,
    139,
    134,
    110,
    104,
    108,
    81,
    89,
    81,
    59,
    77,
    59,
    60,
    59,
    58,
    63,
    40,
    38,
    38,
    41,
    40,
    55,
    36,
    27,
    29,
    42,
    27,
    40,
    34,
    26,
    25,
    25,
    22,
    26,
    28,
    25,
    614,
]

const mean = nums => {
  const num = nums.reduce((a, b, idx) => a+b, 0)
  const sum = nums.reduce((a, b, idx) => a+b*idx, 0)
  console.log('sum', sum, num)
  console.log('mean', sum / num)
}

mean(size_counts)
mean(bucket_items_50)
mean(bucket_items_10)