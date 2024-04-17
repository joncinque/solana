use {
    crate::{
        ledger::get_ledger_from_info,
        locator::{Locator, Manufacturer},
        remote_wallet::{
            RemoteWallet, RemoteWalletError, RemoteWalletInfo, RemoteWalletManager,
            RemoteWalletType,
        },
        trezor::get_trezor_from_info,
    },
    solana_derivation_path::DerivationPath,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::{Signer, SignerError},
};

pub struct RemoteKeypair {
    pub wallet_type: RemoteWalletType,
    pub derivation_path: DerivationPath,
    pub pubkey: Pubkey,
    pub path: String,
}

impl RemoteKeypair {
    pub fn new(
        wallet_type: RemoteWalletType,
        derivation_path: DerivationPath,
        confirm_key: bool,
        path: String,
    ) -> Result<Self, RemoteWalletError> {
        let pubkey = match &wallet_type {
            RemoteWalletType::Ledger(wallet) => wallet.get_pubkey(&derivation_path, confirm_key)?,
            RemoteWalletType::Trezor(wallet) => wallet.get_pubkey(&derivation_path, confirm_key)?,
        };

        Ok(Self {
            wallet_type,
            derivation_path,
            pubkey,
            path,
        })
    }
}

impl Signer for RemoteKeypair {
    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        Ok(self.pubkey)
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        match &self.wallet_type {
            RemoteWalletType::Ledger(wallet) => wallet
                .sign_message(&self.derivation_path, message)
                .map_err(|e| e.into()),
            RemoteWalletType::Trezor(wallet) => wallet
                .sign_message(&self.derivation_path, message)
                .map_err(|e| e.into()),
        }
    }

    fn is_interactive(&self) -> bool {
        true
    }
}

pub fn generate_remote_keypair(
    locator: Locator,
    derivation_path: DerivationPath,
    wallet_manager: &RemoteWalletManager,
    confirm_key: bool,
    keypair_name: &str,
) -> Result<RemoteKeypair, RemoteWalletError> {
    let remote_wallet_info = RemoteWalletInfo::parse_locator(locator);
    match remote_wallet_info.manufacturer {
        Manufacturer::Ledger => {
            let ledger = get_ledger_from_info(remote_wallet_info, keypair_name, wallet_manager)?;
            let path = format!("{}{}", ledger.pretty_path, derivation_path.get_query());
            Ok(RemoteKeypair::new(
                RemoteWalletType::Ledger(ledger),
                derivation_path,
                confirm_key,
                path,
            )?)
        }
        Manufacturer::Trezor => {
            let trezor = get_trezor_from_info(remote_wallet_info, keypair_name, wallet_manager)?;
            let path = format!("{}{}", trezor.pretty_path, derivation_path.get_query());
            Ok(RemoteKeypair::new(
                RemoteWalletType::Trezor(trezor),
                derivation_path,
                confirm_key,
                path,
            )?)
        }
        _ => Err(RemoteWalletError::DeviceTypeMismatch),
    }
}
