
let settlement = match direction {
                                TokenDirection::Up => {
                                    if binance_price > market.strike_price_binance {
                                        1.0
                                    } else {
                                        0.0
                                    }
                                }
                                TokenDirection::Down => {
                                    if binance_price <= market.strike_price_binance {
                                        1.0
                                    } else {
                                        0.0
                                    }
                                }
                            };