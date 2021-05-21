#![no_std]
#![no_main]

use embedded_graphics as eg;
use panic_halt as _;
use wio_terminal as wio;

use cortex_m::peripheral::NVIC;
use eg::{fonts::*, pixelcolor::*, prelude::*, primitives::*, style::*};
use wio::hal::clock::GenericClockController;
use wio::hal::delay::Delay;
use wio::hal::timer::TimerCounter;
use wio::hal::pwm::Channel;
use wio::pac::{CorePeripherals, Peripherals, interrupt, TC3};
use wio::prelude::*;
use core::fmt::Write;
use wio::{entry, Pins};

use heapless::String;
use heapless::consts::*;

struct Ctx {
    tc3: TimerCounter<TC3>,
    timer_counter: [u32;7], //user
}
static mut CTX: Option<Ctx> = None;

#[entry]
fn main() -> ! {
    //レジスタ叩いて5V出力
    /*参考資料
    組込みRust p150
    SAM_D5x_E5x_Family_Data_Sheet_DS60001507G.pdf p51,807
    */
    unsafe{
        const PA_DIRSET: u32=0x41008108;
        *(PA_DIRSET as *mut u32) = 1<<14;
        const PA_OUTSET: u32 = 0x41008118;
        *(PA_OUTSET as *mut u32)=1<<14;
    }

    //ハードウェア初期設定
    let mut peripherals = Peripherals::take().unwrap();
    let core = CorePeripherals::take().unwrap();
    let mut clocks = GenericClockController::with_external_32kosc(
        peripherals.GCLK,
        &mut peripherals.MCLK,
        &mut peripherals.OSC32KCTRL,
        &mut peripherals.OSCCTRL,
        &mut peripherals.NVMCTRL,
    );
    let mut delay = Delay::new(core.SYST, &mut clocks);
    let mut sets = Pins::new(peripherals.PORT).split();

    //ボタン初期設定
    let button1 = sets.buttons.button1.into_floating_input(&mut sets.port);
    let mut button1_pressed=false;
    let button2 = sets.buttons.button2.into_floating_input(&mut sets.port);
    let mut button2_pressed=false;
    let button3 = sets.buttons.button3.into_floating_input(&mut sets.port);
    let mut button3_pressed=false;

    //PWM/ブザーの設定
    let mut buzzer = sets.buzzer.init(
        &mut clocks,
        peripherals.TCC0,
        &mut peripherals.MCLK,
        &mut sets.port,
    );
    buzzer.set_period(1000.hz());
    let max_duty = buzzer.get_max_duty();
    buzzer.set_duty(Channel::_4,max_duty/2);
    buzzer.disable(Channel::_4);
    
    //UARTドライバオブジェクトの初期化
    let mut serial =sets.uart.init(
        &mut clocks,
        9600.hz(),
        peripherals.SERCOM2,
        &mut peripherals.MCLK,
        &mut sets.port,
    );
    
    //ユーザーが調整する部分
    const CO2_RANGE_PPM :u32  =6;//user
    let mut co2_range_time=0;
    const CO2_RANGE_TIME_ARRAY:[u32;7]=[1,5,30,60,720,1440,4320];//user
    const SELF_CAL  :bool =true;//user
    //CO2センサーまわりの定義
    const READ_CO2:[u8;9]=[0xFF, 0x01, 0x86, 0x00, 0x00, 0x00, 0x00, 0x00, 0x79];
    const SELF_CAL_ON:[u8;9]=[0xFF, 0x01, 0x79, 0xA0, 0x00, 0x00, 0x00, 0x00, 0xE6];
    const SELF_CAL_OFF:[u8;9] = [0xFF, 0x01, 0x79, 0x00, 0x00, 0x00, 0x00, 0x00, 0x86];
    let mut readdata:[u8;9]=[0;9];
    let mut co2_history:[[u32;320];CO2_RANGE_TIME_ARRAY.len()]=[[400;320];CO2_RANGE_TIME_ARRAY.len()];
    let mut co2_graf:[u32;320]=[0;320];

    //割り込み設定
    let gclk5 = clocks
        .get_gclk(wio::pac::gclk::pchctrl::GEN_A::GCLK5)
        .unwrap(); 
    let timer_clock = clocks.tc2_tc3(&gclk5).unwrap();
    let mut tc3 = TimerCounter::tc3_(
        &timer_clock,
        peripherals.TC3,
        &mut peripherals.MCLK,
    );    
    unsafe{
        NVIC::unmask(interrupt::TC3);
    }   
    tc3.start(1.ms());
    tc3.enable_interrupt();
    unsafe{
        CTX=Some(Ctx{
            tc3,
            timer_counter:[0;CO2_RANGE_TIME_ARRAY.len()],
        });
    }        

    //ディスプレイドライバの初期化
    let (mut display, mut _backlight)=sets
        .display
        .init(
            &mut clocks,
            peripherals.SERCOM7,
            &mut peripherals.MCLK,
            &mut sets.port,
            58.mhz(),
            &mut delay,
        )
        .unwrap();
    let mut display_backlight = true;

    //LCDを黒色で塗りつぶす
    let style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb565::BLACK)
        .build();
    let background = 
        Rectangle::new(Point::new(0,0),Point::new(319,239))
            .into_styled(style);
    background.draw(&mut display).unwrap();
    
    //縦軸の数値を表示
    Text::new(" 400",Point::new(0,224))
        .into_styled(TextStyle::new(Font12x16,Rgb565::GREEN))
        .draw(&mut display)
        .unwrap();
    Text::new(" 700",Point::new(0,224-(300/CO2_RANGE_PPM as i32)))
        .into_styled(TextStyle::new(Font12x16,Rgb565::YELLOW))
        .draw(&mut display)
        .unwrap();
    Text::new("1000",Point::new(0,224-(600/CO2_RANGE_PPM as i32)))
        .into_styled(TextStyle::new(Font12x16,Rgb565::RED))
        .draw(&mut display)
        .unwrap();

    //キャリブレーションの設定の表示
    let self_cal_str = if SELF_CAL {
        "Self-Cal: ON"
    } else {
        "Self-Cal: OFF"
    };
    Text::new(self_cal_str,Point::new(210,8))
        .into_styled(TextStyle::new(Font8x16,Rgb565::WHITE))
        .draw(&mut display)
        .unwrap();

    //電源投入直後はセンサーが通信不可
    Text::new("loading...",Point::new(50,30))
        .into_styled(TextStyle::new(Font24x32,Rgb565::WHITE))
        .draw(&mut display)
        .unwrap();
    delay.delay_ms(4000u16);
    if SELF_CAL {
        for c in SELF_CAL_ON.iter(){
            nb::block!(serial.write(*c)).unwrap();
        }
    }else{
        for c in SELF_CAL_OFF.iter(){
            nb::block!(serial.write(*c)).unwrap();
        }
    }
    //selfcalの設定直後はセンサーが通信不可
    delay.delay_ms(1000u16);
    let textback = 
        Rectangle::new(Point::new(30,30),Point::new(292,62))
            .into_styled(style);
    textback.draw(&mut display).unwrap();

    //メインループ開始
    loop {
        //センサー読み取り
        for c in READ_CO2.iter(){
            nb::block!(serial.write(*c)).unwrap();
        }
        //delay.delay_ms(10u16);
        for i in 0..9 {
            readdata[i]=nb::block!(serial.read()).unwrap();
        }

        let co2ppm_int:u32 = (readdata[4] as u32)*256+readdata[5]as u32;

        //全時間レンジの配列にデータを格納
        for i in 0..CO2_RANGE_TIME_ARRAY.len() {
            unsafe{
                //それぞれのレンジで時間が来たら配列に書き込み
                if  CTX.as_ref().unwrap().timer_counter[i]>(CO2_RANGE_TIME_ARRAY[i] *60*1000/273) {
                    CTX.as_mut().unwrap().timer_counter[i]=0;
                    
                    //まずは配列にデータを格納
                    for j in 47..320{
                        co2_history[i][(j as usize)] = if j==319 {
                            if co2ppm_int < (400 + 140*CO2_RANGE_PPM) {
                                co2ppm_int
                            }else{
                                400 + 140*CO2_RANGE_PPM
                            }
                        }else{
                            co2_history[i][(j as usize)+1]
                        };
                    }

                    //co2_range_timeの時間が来ているなら画面を更新
                    if i==co2_range_time {
                        //データ描画準備
                        let mut co2ppm_str = if co2_history[co2_range_time][319] <10 {
                            String::<U32>::from("CO2:   ")
                        } else if co2_history[co2_range_time][319] <100 {
                            String::<U32>::from("CO2:  ")
                        } else if co2_history[co2_range_time][319] <1000 {
                            String::<U32>::from("CO2: ")
                        } else{
                            String::<U32>::from("CO2:")
                        };
                        write!(co2ppm_str,"{}ppm", co2_history[co2_range_time][319]).unwrap();

                        //テキスト色分け・警報処理
                        let text_style = if co2_history[co2_range_time][319] <700 {
                            TextStyle::new(Font24x32,Rgb565::GREEN)
                        } else if co2_history[co2_range_time][319] <1000 {
                            if display_backlight && co2_history[co2_range_time][318]<700{
                                buzzer.enable(Channel::_4);
                                delay.delay_ms(300 as u16);
                                buzzer.disable(Channel::_4);
                                delay.delay_ms(100 as u16);
                                buzzer.enable(Channel::_4);
                                delay.delay_ms(300 as u16);
                                buzzer.disable(Channel::_4);
                            }
                            TextStyle::new(Font24x32,Rgb565::YELLOW)
                        } else {
                            if display_backlight && co2_history[co2_range_time][318]<1000{
                                buzzer.enable(Channel::_4);
                                delay.delay_ms(300 as u16);
                                buzzer.disable(Channel::_4);
                                delay.delay_ms(100 as u16);
                                buzzer.enable(Channel::_4);
                                delay.delay_ms(300 as u16);
                                buzzer.disable(Channel::_4);
                                delay.delay_ms(100 as u16);
                                buzzer.enable(Channel::_4);
                                delay.delay_ms(300 as u16);
                                buzzer.disable(Channel::_4);
                            }
                            TextStyle::new(Font24x32,Rgb565::RED)
                        };

                        //文字描画
                        //文字更新用のバック(前の数値を隠すやつ)を用意する
                        let databack = 
                            Rectangle::new(Point::new(126,30),Point::new(217,57))
                                .into_styled(style);
                        databack.draw(&mut display).unwrap();
                        
                        Text::new(co2ppm_str.as_str(),Point::new(30,30))
                            .into_styled(text_style)
                            .draw(&mut display)
                            .unwrap();
                        
                        //グラフ描画(古いの消すのと新しいのを書くのを同時に)
                        for i in 47..320 {
                            //前のグラフを消す
                            Pixel(Point::new(i, 230- (((co2_graf[(i as usize)] as i32)-400)/CO2_RANGE_PPM as i32) ), Rgb565::BLACK)
                                .draw(&mut display)
                                .unwrap();
                                
                            //co2_history[co2_range_time]からグラフを書く
                            Pixel(Point::new(i, 230- (((co2_history[co2_range_time][(i as usize)] as i32)-400)/CO2_RANGE_PPM as i32) ), Rgb565::WHITE)
                                .draw(&mut display)
                                .unwrap();
                        }
                        //新しいグラフを前のグラフとしてデータを残す
                        for i in 47..320 {
                            co2_graf[(i as usize)]= co2_history[co2_range_time][(i as usize)];
                        }
                    }
                }
            }
        }        
        
        //バックライトON/OFF
        if button1.is_low().unwrap() {
            if button1_pressed==false {
                if display_backlight {
                    _backlight.set_low().unwrap();
                    display_backlight = false;
                }else{
                    _backlight.set_high().unwrap();
                    display_backlight = true;
                }
            }
            button1_pressed=true;                
        }else{
            button1_pressed=false;  
        }
        
        //時間レンジ設定
        if button2.is_low().unwrap() {
            if button2_pressed==false && co2_range_time != CO2_RANGE_TIME_ARRAY.len()-1 {
                co2_range_time+=1;
                //レンジの設定の表示のリセット
                let rangeback = 
                    Rectangle::new(Point::new(0,0),Point::new(209,30))
                        .into_styled(style);
                rangeback.draw(&mut display).unwrap();  
                //すぐにグラフ更新                
                unsafe{
                    let ctx=CTX.as_mut().unwrap();
                    ctx.timer_counter[co2_range_time]=(CO2_RANGE_TIME_ARRAY[co2_range_time] *60*1000/273)+1;
                }
            }
            button2_pressed=true;                
        }else{
            button2_pressed=false;  
        }
        if button3.is_low().unwrap() {
            if button3_pressed==false && co2_range_time != 0{
                co2_range_time-=1;
                //レンジの設定の表示のリセット
                let rangeback = 
                    Rectangle::new(Point::new(0,0),Point::new(209,30))
                        .into_styled(style);
                rangeback.draw(&mut display).unwrap();
                //すぐにグラフ更新   
                unsafe{
                    let ctx=CTX.as_mut().unwrap();
                    ctx.timer_counter[co2_range_time]=(CO2_RANGE_TIME_ARRAY[co2_range_time] *60*1000/273)+1;
                }
            }
            button3_pressed=true;                
        }else{
            button3_pressed=false;  
        }
        //レンジの設定の表示
        let mut co2_range_time_array_str = String::<U32>::from("range: ");
        let sdiv:f32 =(CO2_RANGE_TIME_ARRAY[co2_range_time]*60) as f32/ 273 as f32;
        if CO2_RANGE_TIME_ARRAY[co2_range_time] >60{
            write!(co2_range_time_array_str,"{}h {:.2}s/div", CO2_RANGE_TIME_ARRAY[co2_range_time]/60,sdiv).unwrap();
        }else{
            write!(co2_range_time_array_str,"{}min {:.2}s/div", CO2_RANGE_TIME_ARRAY[co2_range_time],sdiv).unwrap();
        }
        Text::new(co2_range_time_array_str.as_str(),Point::new(8,8))
            .into_styled(TextStyle::new(Font8x16,Rgb565::WHITE))
            .draw(&mut display)
            .unwrap();

        //座標チェック
        /*
        Pixel(Point::new(50, 230- ((1000-400)/CO2_RANGE_PPM as i32) ), Rgb565::RED)
            .draw(&mut display)
            .unwrap();
        */
    }
}

// TODO: TC3 の割り込みハンドラを実装する
#[interrupt]
fn TC3(){
    unsafe{
        let ctx=CTX.as_mut().unwrap();
        ctx.tc3.wait().unwrap();
        for i in 0..ctx.timer_counter.len(){
            ctx.timer_counter[i]+=1;
        }
    }
}