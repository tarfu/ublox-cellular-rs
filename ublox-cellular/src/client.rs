use atat::AtatClient;
use core::cell::RefCell;
use embedded_hal::{
    blocking::delay::DelayMs,
    digital::{InputPin, OutputPin},
    timer::CountDown,
};
use heapless::{ArrayLength, Bucket, Pos};

use crate::{
    command::device_lock::GetPinStatus,
    command::device_lock::{responses::PinStatus, types::PinStatusCode},
    command::general::GetCCID,
    command::{
        control::{types::*, *},
        general::responses::CCID,
        mobile_control::{types::*, *},
        network_service::SetRadioAccessTechnology,
        psn::responses::GPRSAttached,
        psn::types::GPRSAttachedState,
        psn::GetGPRSAttached,
        system_features::{types::*, *},
        *,
    },
    config::{Config, NoPin},
    error::Error,
    network::{AtTx, Error as NetworkError, Network},
    services::data::socket::{SocketSet, SocketSetItem},
    state::Event,
    state::RadioAccessNetwork,
    state::StateMachine,
    State,
};
use ip_transport_layer::{types::HexMode, SetHexMode};
use network_service::{
    types::{NetworkRegistrationUrcConfig, RadioAccessTechnologySelected, RatPreferred},
    SetNetworkRegistrationStatus,
};
use psn::{
    types::{EPSNetworkRegistrationUrcConfig, GPRSNetworkRegistrationUrcConfig},
    SetEPSNetworkRegistrationStatus, SetGPRSNetworkRegistrationStatus,
};
use sms::{types::MessageWaitingMode, SetMessageWaitingIndication};

pub struct Device<C, DLY, N, L, RST = NoPin, DTR = NoPin, PWR = NoPin, VINT = NoPin>
where
    C: AtatClient,
    DLY: DelayMs<u32> + CountDown,
    N: 'static
        + ArrayLength<Option<SocketSetItem<L>>>
        + ArrayLength<Bucket<u8, usize>>
        + ArrayLength<Option<Pos>>,
    L: 'static + ArrayLength<u8>,
{
    pub(crate) fsm: StateMachine,
    pub(crate) config: Config<RST, DTR, PWR, VINT>,
    pub(crate) delay: DLY,
    pub(crate) network: Network<C>,
    // Ublox devices can hold a maximum of 6 active sockets
    pub(crate) sockets: Option<RefCell<&'static mut SocketSet<N, L>>>,
}

impl<C, DLY, N, L, RST, DTR, PWR, VINT> Device<C, DLY, N, L, RST, DTR, PWR, VINT>
where
    C: AtatClient,
    DLY: DelayMs<u32> + CountDown,
    DLY::Time: From<u32>,
    RST: OutputPin,
    PWR: OutputPin,
    DTR: OutputPin,
    VINT: InputPin,
    N: ArrayLength<Option<SocketSetItem<L>>>
        + ArrayLength<Bucket<u8, usize>>
        + ArrayLength<Option<Pos>>,
    L: ArrayLength<u8>,
{
    pub fn new(client: C, delay: DLY, config: Config<RST, DTR, PWR, VINT>) -> Self {
        Device {
            fsm: StateMachine::new(),
            config,
            delay,
            network: Network::new(AtTx::new(client, 5)),
            sockets: None,
        }
    }

    pub fn set_socket_storage(&mut self, socket_set: &'static mut SocketSet<N, L>) {
        self.sockets = Some(RefCell::new(socket_set));
    }

    pub(crate) fn initialize(&mut self, leave_pwr_alone: bool) -> Result<(), Error> {
        defmt::info!(
            "Initialising with PWR_ON pin: {:bool} and VInt pin: {:bool}",
            self.config.pwr_pin.is_some(),
            self.config.vint_pin.is_some()
        );

        match self.config.pwr_pin {
            Some(ref mut pwr) if !leave_pwr_alone => {
                pwr.try_set_high().ok();
            }
            _ => {}
        }

        Ok(())
    }

    pub(crate) fn power_on(&mut self) -> Result<(), Error> {
        let vint_value = match self.config.vint_pin {
            Some(ref _vint) => false,
            _ => false,
        };

        if vint_value || self.is_alive(3).is_ok() {
            defmt::debug!("powering on, module is already on, flushing config...");
        } else {
            defmt::debug!("powering on.");
            if let Some(ref mut pwr) = self.config.pwr_pin {
                pwr.try_set_low().ok();
                self.delay
                    .try_delay_ms(crate::module_cfg::constants::PWR_ON_PULL_TIME_MS)
                    .map_err(|_| Error::Busy)?;
                pwr.try_set_high().ok();
            } else {
                // Software restart
                self.restart(false)?;
            }
            self.delay
                .try_delay_ms(crate::module_cfg::constants::BOOT_WAIT_TIME_MS)
                .map_err(|_| Error::Busy)?;
            self.is_alive(10)?;
            // self.network.send_internal(&SetFactoryConfiguration {
            //     fs_op: FSFactoryRestoreType::AllFiles,
            //     nvm_op: NVMFactoryRestoreType::NVMFlashSectors,
            // }, true)?;
        }
        Ok(())
    }

    /// Check that the cellular module is alive.
    ///
    /// See if the cellular module is responding at the AT interface by poking
    /// it with "AT" up to `attempts` times, waiting 1 second for an "OK"
    /// response each time
    pub(crate) fn is_alive(&self, attempts: u8) -> Result<(), Error> {
        let mut error = Error::BaudDetection;
        for _ in 0..attempts {
            match self.network.send_internal(&AT, false) {
                Ok(_) => {
                    return Ok(());
                }
                Err(e) => error = e.into(),
            };
        }
        Err(error)
    }

    pub(crate) fn configure(&self) -> Result<(), Error> {
        if self.config.baud_rate > 230_400_u32 {
            // Needs a way to reconfigure uart baud rate temporarily
            // Relevant issue: https://github.com/rust-embedded/embedded-hal/issues/79
            return Err(Error::_Unknown);

            // self.network.send_internal(
            //     &SetDataRate {
            //         rate: BaudRate::B115200,
            //     },
            //     true,
            // )?;

            // NOTE: On the UART AT interface, after the reception of the "OK" result code for the +IPR command, the DTE
            // shall wait for at least 100 ms before issuing a new AT command; this is to guarantee a proper baud rate
            // reconfiguration.

            // UART end
            // delay(100);
            // UART begin(self.config.baud_rate)

            // self.is_alive()?;
        }

        // Extended errors on
        self.network.send_internal(
            &SetReportMobileTerminationError {
                n: TerminationErrorMode::Verbose,
            },
            false,
        )?;

        // DCD circuit (109) changes in accordance with the carrier
        self.network.send_internal(
            &SetCircuit109Behaviour {
                value: Circuit109Behaviour::ChangesWithCarrier,
            },
            false,
        )?;

        // Ignore changes to DTR
        self.network.send_internal(
            &SetCircuit108Behaviour {
                value: Circuit108Behaviour::Ignore,
            },
            false,
        )?;

        // Switch off UART power saving until it is integrated into this API
        self.network.send_internal(
            &SetPowerSavingControl {
                mode: PowerSavingMode::Disabled,
                timeout: None,
            },
            false,
        )?;

        if self.config.hex_mode {
            self.network.send_internal(
                &SetHexMode {
                    hex_mode_disable: HexMode::Enabled,
                },
                false,
            )?;
        } else {
            self.network.send_internal(
                &SetHexMode {
                    hex_mode_disable: HexMode::Disabled,
                },
                false,
            )?;
        }

        // self.network.send_internal(&general::IdentificationInformation { n: 9 }, true)?;

        // Stay in airplane mode until commanded to register
        self.network.send_internal(
            &SetModuleFunctionality {
                fun: Functionality::AirplaneMode,
                rst: None,
            },
            false,
        )?;

        // Tell module whether we support flow control
        if self.config.flow_control {
            self.network.send_internal(
                &SetFlowControl {
                    value: FlowControl::RtsCts,
                },
                false,
            )?;
        } else {
            self.network.send_internal(
                &SetFlowControl {
                    value: FlowControl::Disabled,
                },
                false,
            )?;
        }

        // Disable Message Waiting URCs (UMWI)
        self.network.send_internal(
            &SetMessageWaitingIndication {
                mode: MessageWaitingMode::Disabled,
            },
            false,
        )?;

        Ok(())
    }

    #[inline]
    pub(crate) fn restart(&self, sim_reset: bool) -> Result<(), Error> {
        if sim_reset {
            self.network.send_internal(
                &SetModuleFunctionality {
                    fun: Functionality::SilentResetWithSimReset,
                    rst: None,
                },
                false,
            )?;
        } else {
            self.network.send_internal(
                &SetModuleFunctionality {
                    fun: Functionality::SilentReset,
                    rst: None,
                },
                false,
            )?;
        }
        Ok(())
    }

    pub(crate) fn enable_registration_urcs(&self) -> Result<(), Error> {
        self.network.send_internal(
            &SetNetworkRegistrationStatus {
                n: NetworkRegistrationUrcConfig::UrcDisabled,
            },
            true,
        )?;

        self.network.send_internal(
            &SetGPRSNetworkRegistrationStatus {
                n: GPRSNetworkRegistrationUrcConfig::UrcVerbose,
            },
            true,
        )?;

        self.network.send_internal(
            &SetEPSNetworkRegistrationStatus {
                n: EPSNetworkRegistrationUrcConfig::UrcVerbose,
            },
            true,
        )?;
        Ok(())
    }

    pub fn spin(&mut self) -> nb::Result<(), Error> {
        self.network.handle_urc().ok();

        while let Some(event) = self
            .network
            .get_event()
            .map_err(|e| nb::Error::Other(e.into()))?
        {
            match event {
                Event::Disconnected(cid) => {
                    defmt::info!("[EVENT] Disconnected, {:?}", cid);
                    // FIXME: Use cid info to only terminate a single cid
                    self.fsm.set_state(State::Init);
                    self.network
                        .clear_events()
                        .map_err(|e| nb::Error::Other(e.into()))?;
                }
                Event::CellularRegistrationStatusChanged(reg_type, status) => {
                    defmt::info!(
                        "[EVENT] CellularRegistrationStatusChanged {:?} {:?}",
                        reg_type,
                        status
                    );
                    if matches!(
                        self.fsm.get_state(),
                        State::SignalQuality
                            | State::RegisteringNetwork
                            | State::AttachingNetwork
                            | State::Connected
                    ) && matches!(
                        reg_type,
                        RadioAccessNetwork::Utran | RadioAccessNetwork::Eutran
                    ) && status.is_registered().is_some()
                    {
                        self.fsm.set_state(State::RegisteringNetwork);
                        self.network
                            .clear_events()
                            .map_err(|e| nb::Error::Other(e.into()))?;
                    }
                }
                Event::CellularRadioAccessTechnologyChanged(reg_type, rat) => {
                    defmt::info!(
                        "[EVENT] CellularRadioAccessTechnologyChanged {:?} {:?}",
                        reg_type,
                        rat
                    );
                    // TODO: What to do here??
                }
                Event::CellularCellIDChanged(cell_id) => {
                    defmt::info!(
                        "[EVENT] CellularCellIDChanged {:str}",
                        cell_id.unwrap_or_default().as_str()
                    );
                }
            }
        }

        if self.fsm.is_retry() {
            if let Err(nb::Error::WouldBlock) = self.delay.try_wait() {
                return Err(nb::Error::WouldBlock);
            }
        }

        let new_state = match self.fsm.get_state() {
            State::Init => match self.initialize(true) {
                Ok(()) => Ok(State::PowerOn),
                Err(_) => Err(State::Init),
            },
            State::PowerOn => match self.power_on() {
                Ok(()) => Ok(State::Configure),
                Err(_) => Err(State::PowerOn),
            },
            State::Configure => match self.configure() {
                Ok(()) => Ok(State::DeviceReady),
                Err(_) => Err(State::PowerOn),
            },
            State::DeviceReady => {
                self.network
                    .send_internal(
                        &SetRadioAccessTechnology {
                            selected_act: RadioAccessTechnologySelected::GsmUmtsLte(
                                RatPreferred::Lte,
                                RatPreferred::Utran,
                            ),
                        },
                        true,
                    )
                    .map_err(|e| nb::Error::Other(e.into()))?;

                // Now come out of airplane mode
                self.network
                    .send_internal(
                        &SetModuleFunctionality {
                            fun: Functionality::Full,
                            rst: None,
                        },
                        true,
                    )
                    .map_err(|e| nb::Error::Other(e.into()))?;

                Ok(State::SimPin)
            }
            State::SimPin => {
                self.enable_registration_urcs()?;

                let PinStatus { code } = self
                    .network
                    .send_internal(&GetPinStatus, true)
                    .map_err(|e| nb::Error::Other(e.into()))?;

                if code == PinStatusCode::Ready {
                    self.network.attached.set(false);
                    self.network.pdp_context_active.set(false);

                    // TODO: check if context was already activated
                    // let PDPContextState { status } =
                    //     self.network.send_internal(&GetPDPContextState, true)?;
                    if false {
                        defmt::debug!("Active context found");
                        self.network.pdp_context_active.set(true);
                    }

                    // Check if modem is already attached to a network
                    if let GPRSAttached {
                        state: GPRSAttachedState::Attached,
                    } = self
                        .network
                        .send_internal(&GetGPRSAttached, true)
                        .map_err(|e| nb::Error::Other(e.into()))?
                    {
                        defmt::debug!("Cellular already attached");
                        self.network.attached.set(true);
                    }

                    // FIXME:
                    // self.network.send_internal(
                    //     &mobile_control::SetAutomaticTimezoneUpdate {
                    //         on_off: AutomaticTimezone::EnabledLocal,
                    //     },
                    //     true,
                    // )?;

                    // if packet domain event reporting is not set it's not a
                    // stopper. We might lack some events when we are dropped
                    // from the network.
                    if self
                        .network
                        .set_packet_domain_event_reporting(true)
                        .is_err()
                    {
                        defmt::warn!("Packet domain event reporting set failed");
                    }

                    Ok(State::SignalQuality)
                } else {
                    // TODO: Handle SIM Pin here
                    defmt::error!("PIN status not ready!!");
                    Err(State::PowerOn)
                }
            }
            State::SignalQuality => {
                if let Ok(CCID { ccid }) = self.network.send_internal(&GetCCID, true) {
                    defmt::info!("CCID: {:?}", ccid.to_le_bytes());
                }
                Ok(State::RegisteringNetwork)
            }
            State::RegisteringNetwork => match self.network.register(None) {
                Ok(_) => Ok(State::AttachingNetwork),
                Err(nb::Error::Other(NetworkError::RegistrationDenied)) => {
                    self.restart(true)?;
                    self.delay
                        .try_delay_ms(crate::module_cfg::constants::BOOT_WAIT_TIME_MS)
                        .map_err(|_| Error::Busy)?;

                    self.fsm.set_max_retry_attempts(0);
                    Err(State::PowerOn)
                }
                Err(_) => Err(State::PowerOn),
            },
            State::AttachingNetwork => match self.network.attach() {
                Ok(_) => Ok(State::Connected),
                Err(_) => Err(State::PowerOn),
            },
            State::Connected => {
                // Reset the retry attempts on connected, as this
                // essentially is a success path.
                self.fsm.reset();
                return Ok(());
            }
        };

        match new_state {
            Ok(new_state) => self.fsm.set_state(new_state),
            Err(err_state) => {
                if let nb::Error::Other(Error::StateTimeout) =
                    self.fsm.retry_or_fail(&mut self.delay)
                {
                    self.fsm.set_state(err_state);
                }
            }
        }

        Err(nb::Error::WouldBlock)
    }

    pub fn send_at<A: atat::AtatCmd>(&self, cmd: &A) -> Result<A::Response, Error> {
        // At any point after init state, we should be able to fully send AT
        // commands.
        if self.fsm.get_state() == State::Init {
            return Err(Error::Uninitialized);
        }

        Ok(self.network.send_internal(cmd, true)?)
    }
}
