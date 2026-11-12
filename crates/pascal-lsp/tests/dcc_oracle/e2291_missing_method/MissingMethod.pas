unit MissingMethod;
interface
type
  TObject = class
  end;
  IRequired = interface(IInterface)
    procedure Required;
  end;
  TImplementation = class(TObject, IRequired)
  end;
implementation
end.
